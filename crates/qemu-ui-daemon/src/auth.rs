//! Authentication (plan.md §13, M6-01): the single admin account, browser
//! sessions, CSRF, login rate limiting, API tokens and the authorization
//! funnel [`can`].
//!
//! Before first-run setup the daemon behaves as in M1–M5: loopback only,
//! same-origin browsers or the local bootstrap token. Setup creates the admin
//! account; from then on browsers log in (session cookie + CSRF token) and
//! scripts use API tokens. Only after setup may the daemon listen beyond
//! loopback.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, App};
use crate::store::Store;

pub const SESSION_COOKIE: &str = "qui_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Sessions end after this long without requests…
const IDLE: u64 = 12 * 3600;
/// …and after this long regardless.
const ABSOLUTE: u64 = 7 * 24 * 3600;
const MIN_PASSWORD: usize = 8;
/// Failed logins allowed before backoff starts.
const FREE_ATTEMPTS: u32 = 5;
const MAX_LOCKOUT: Duration = Duration::from_secs(300);
const EXPERT_KEY: &str = "settings:expert-mode";

/// Who is acting.
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
    /// The name recorded in the audit log.
    pub fn label(&self) -> String {
        match self {
            Actor::Anonymous => "anonymous".into(),
            Actor::LocalSetup => "local (before setup)".into(),
            Actor::Bootstrap => "api-token".into(),
            Actor::Session => "admin".into(),
            Actor::Token(id) => format!("token:{id}"),
        }
    }
}

/// What an actor wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Public endpoints: health, auth state, login, setup.
    Public,
    /// Everything ordinary: read and manage VMs, storage, networks.
    Use,
    /// Account management: password, API tokens, settings.
    Account,
    /// Expert surfaces (passthrough edits, QMP console): root-equivalent.
    Expert,
}

/// The one authorization decision (single-user: every authenticated actor
/// may use the daemon; account changes need the admin's own session or the
/// local bootstrap token; expert surfaces also need expert mode on).
pub fn can(actor: &Actor, action: Action, expert_mode: bool) -> bool {
    match action {
        Action::Public => true,
        Action::Use => !matches!(actor, Actor::Anonymous),
        Action::Account => matches!(actor, Actor::Session | Actor::Bootstrap | Actor::LocalSetup),
        Action::Expert => {
            expert_mode && matches!(actor, Actor::Session | Actor::Bootstrap | Actor::LocalSetup)
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random_hex(bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn hash_password(pw: &str) -> Result<String> {
    let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>()).map_err(|e| anyhow!("{e}"))?;
    Ok(Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow!("{e}"))?
        .to_string())
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

#[derive(Default)]
struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
}

pub struct Auth {
    store: Store,
    /// Cookies get `Secure` when the daemon is served over TLS.
    pub tls: bool,
    /// Listening beyond loopback (allowed only after setup).
    pub lan: bool,
    attempts: Mutex<HashMap<Option<IpAddr>, Attempts>>,
}

/// A session found from a request's cookie.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub token_hash: String,
    pub csrf: String,
}

impl Auth {
    pub fn new(store: Store, tls: bool, lan: bool) -> Arc<Self> {
        Arc::new(Auth {
            store,
            tls,
            lan,
            attempts: Mutex::default(),
        })
    }

    /// Served over HTTPS (main's TLS layer), or forced for tests.
    pub fn is_tls(&self) -> bool {
        self.tls || crate::server_config::tls()
    }

    pub fn setup_complete(&self) -> bool {
        self.store.admin().ok().flatten().is_some()
    }

    pub fn expert_mode(&self) -> bool {
        self.store.get_kv(EXPERT_KEY).ok().flatten().as_deref() == Some("on")
    }

    pub fn set_expert_mode(&self, on: bool) -> Result<()> {
        self.store.put_kv(EXPERT_KEY, if on { "on" } else { "off" })
    }

    /// The session named by the request's cookie, if valid (and refreshed).
    pub fn session(&self, headers: &HeaderMap) -> Option<SessionInfo> {
        let token = cookie(headers, SESSION_COOKIE)?;
        let h = sha256_hex(&token);
        let (csrf, created, seen) = self.store.session(&h).ok()??;
        let t = now();
        if t.saturating_sub(seen) > IDLE || t.saturating_sub(created) > ABSOLUTE {
            let _ = self.store.delete_session(&h);
            return None;
        }
        if t > seen + 60 {
            let _ = self.store.touch_session(&h, t);
        }
        Some(SessionInfo {
            token_hash: h,
            csrf,
        })
    }

    /// Start a session; returns (cookie token, csrf).
    fn start_session(&self) -> Result<(String, String)> {
        let token = random_hex(32);
        let csrf = random_hex(32);
        self.store
            .insert_session(&sha256_hex(&token), &csrf, now())?;
        Ok((token, csrf))
    }

    pub fn api_token(&self, token: &str) -> Option<String> {
        self.store
            .use_api_token(&sha256_hex(token), now())
            .ok()
            .flatten()
    }

    /// Seconds to wait before another login attempt from `ip`.
    fn locked(&self, ip: Option<IpAddr>) -> Option<u64> {
        let a = self.attempts.lock().unwrap();
        let until = a.get(&ip)?.locked_until?;
        let left = until.checked_duration_since(Instant::now())?;
        Some(left.as_secs().max(1))
    }

    fn login_failed(&self, ip: Option<IpAddr>) {
        let mut map = self.attempts.lock().unwrap();
        let a = map.entry(ip).or_default();
        a.failures += 1;
        if a.failures >= FREE_ATTEMPTS {
            let exp = (a.failures - FREE_ATTEMPTS).min(16);
            let wait = Duration::from_secs(1u64 << exp).min(MAX_LOCKOUT);
            a.locked_until = Some(Instant::now() + wait);
        }
    }

    fn login_ok(&self, ip: Option<IpAddr>) {
        self.attempts.lock().unwrap().remove(&ip);
    }

    fn session_cookie(&self, token: &str, max_age: u64) -> HeaderValue {
        let secure = if self.is_tls() { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
        ))
        .expect("cookie is ASCII")
    }
}

/// A cookie's value from the `Cookie` headers.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// The peer address, when the server records it.
pub fn peer_ip(req: &Request) -> Option<IpAddr> {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip())
}

// ---------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/auth/state", get(state))
        .route("/auth/setup", post(setup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/password", post(change_password))
        .route("/auth/tokens", get(list_tokens).post(create_token))
        .route("/auth/tokens/{id}", delete(revoke_token))
        .route("/settings/expert", put(set_expert))
}

/// Paths reachable without credentials (relative to `/api/v1`).
pub fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/health" | "/auth/state" | "/auth/setup" | "/auth/login"
    )
}

fn deny(code: StatusCode, msg: &str) -> ApiError {
    ApiError(code, msg.into())
}

fn require(app: &App, actor: &Actor, action: Action) -> Result<(), ApiError> {
    if can(actor, action, app.auth.expert_mode()) {
        Ok(())
    } else if action == Action::Expert && !app.auth.expert_mode() {
        Err(deny(
            StatusCode::FORBIDDEN,
            "expert mode is off; enable it in Settings → Security",
        ))
    } else {
        Err(deny(StatusCode::FORBIDDEN, "not allowed for this login"))
    }
}

#[derive(Serialize)]
struct AuthState {
    setup_complete: bool,
    authenticated: bool,
    actor: String,
    username: Option<String>,
    /// CSRF token for this browser session (send as `X-CSRF-Token`).
    csrf: Option<String>,
    expert_mode: bool,
    tls: bool,
}

async fn state(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
) -> Json<AuthState> {
    let s = app.auth.session(&headers);
    Json(AuthState {
        setup_complete: app.auth.setup_complete(),
        authenticated: actor != Actor::Anonymous,
        actor: actor.label(),
        username: app
            .supervisor
            .store
            .admin()
            .ok()
            .flatten()
            .map(|a| a.username),
        csrf: s.map(|s| s.csrf),
        expert_mode: app.auth.expert_mode(),
        tls: app.auth.is_tls(),
    })
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

fn logged_in(app: &App, token: &str, csrf: String) -> Response {
    let mut r = (StatusCode::OK, Json(json!({"csrf": csrf}))).into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, app.auth.session_cookie(token, ABSOLUTE));
    r
}

async fn setup(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    Extension(Loopback(local)): Extension<Loopback>,
    Json(c): Json<Credentials>,
) -> Result<Response, ApiError> {
    if app.auth.setup_complete() {
        return Err(deny(StatusCode::CONFLICT, "setup is already complete"));
    }
    if !local {
        return Err(deny(
            StatusCode::FORBIDDEN,
            "first-run setup is only possible from this machine",
        ));
    }
    let username = c.username.trim();
    if username.is_empty() || username.len() > 64 || username.chars().any(char::is_control) {
        return Err(deny(
            StatusCode::UNPROCESSABLE_ENTITY,
            "choose a user name (1-64 printable characters)",
        ));
    }
    if c.password.chars().count() < MIN_PASSWORD {
        return Err(deny(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the password needs at least {MIN_PASSWORD} characters"),
        ));
    }
    let pw = c.password.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&pw))
        .await
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    app.supervisor
        .store
        .create_admin(username, &hash, now())
        .map_err(|_| deny(StatusCode::CONFLICT, "setup is already complete"))?;
    crate::audit::record(&actor.label(), "auth.setup", username, json!({}));
    let (token, csrf) = app
        .auth
        .start_session()
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    Ok(logged_in(&app, &token, csrf))
}

/// Whether the request came from this machine (loopback `Host` and, when
/// known, a loopback peer). Set by the guard.
#[derive(Debug, Clone, Copy)]
pub struct Loopback(pub bool);

/// The client's address as the guard saw it (for rate limiting).
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);

async fn login(
    State(app): State<Arc<App>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(c): Json<Credentials>,
) -> Result<Response, ApiError> {
    if let Some(wait) = app.auth.locked(ip) {
        let mut r = deny(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("too many failed logins; try again in {wait} s"),
        )
        .into_response();
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(wait));
        return Ok(r);
    }
    let Some(admin) = app.supervisor.store.admin().ok().flatten() else {
        return Err(deny(StatusCode::CONFLICT, "complete first-run setup first"));
    };
    let pw = c.password.clone();
    let hash = admin.pw_hash.clone();
    let ok_pw = tokio::task::spawn_blocking(move || verify_password(&pw, &hash))
        .await
        .unwrap_or(false);
    let ok = ok_pw && c.username.trim() == admin.username;
    let who = Actor::Anonymous;
    if !ok {
        app.auth.login_failed(ip);
        crate::audit::record(
            &who.label(),
            "auth.login-failed",
            c.username.trim(),
            json!({"ip": ip.map(|i| i.to_string())}),
        );
        return Err(deny(
            StatusCode::UNAUTHORIZED,
            "wrong user name or password",
        ));
    }
    app.auth.login_ok(ip);
    crate::audit::record(
        &Actor::Session.label(),
        "auth.login",
        &admin.username,
        json!({"ip": ip.map(|i| i.to_string())}),
    );
    let (token, csrf) = app
        .auth
        .start_session()
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    Ok(logged_in(&app, &token, csrf))
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Some(s) = app.auth.session(&headers) {
        let _ = app.supervisor.store.delete_session(&s.token_hash);
        crate::audit::record(&Actor::Session.label(), "auth.logout", "", json!({}));
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut()
        .insert(header::SET_COOKIE, app.auth.session_cookie("", 0));
    r
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

async fn change_password(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    Json(b): Json<PasswordChange>,
) -> Result<StatusCode, ApiError> {
    require(&app, &actor, Action::Account)?;
    let admin = app
        .supervisor
        .store
        .admin()
        .ok()
        .flatten()
        .ok_or_else(|| deny(StatusCode::CONFLICT, "complete first-run setup first"))?;
    if b.new.chars().count() < MIN_PASSWORD {
        return Err(deny(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the password needs at least {MIN_PASSWORD} characters"),
        ));
    }
    let (cur, hash) = (b.current.clone(), admin.pw_hash.clone());
    if !tokio::task::spawn_blocking(move || verify_password(&cur, &hash))
        .await
        .unwrap_or(false)
    {
        return Err(deny(StatusCode::FORBIDDEN, "the current password is wrong"));
    }
    let new = b.new.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&new))
        .await
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    app.supervisor
        .store
        .set_admin_password(&hash)
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    // Other browsers are logged out; this one stays.
    let keep = app.auth.session(&headers).map(|s| s.token_hash);
    let _ = app.supervisor.store.delete_sessions_except(keep.as_deref());
    crate::audit::record(
        &actor.label(),
        "auth.password-changed",
        &admin.username,
        json!({}),
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct TokenInfo {
    id: String,
    name: String,
    created_at: u64,
    last_used: Option<u64>,
}

async fn list_tokens(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
) -> Result<Json<Vec<TokenInfo>>, ApiError> {
    require(&app, &actor, Action::Account)?;
    let rows = app
        .supervisor
        .store
        .list_api_tokens()
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TokenInfo {
                id: r.id,
                name: r.name,
                created_at: r.created_at,
                last_used: r.last_used,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

async fn create_token(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    Json(b): Json<NewToken>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    require(&app, &actor, Action::Account)?;
    let name = b.name.trim();
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err(deny(
            StatusCode::UNPROCESSABLE_ENTITY,
            "name the token (1-64 printable characters)",
        ));
    }
    let id = random_hex(6);
    let token = format!("qui_{}", random_hex(32));
    app.supervisor
        .store
        .insert_api_token(&id, name, &sha256_hex(&token), now())
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    crate::audit::record(
        &actor.label(),
        "auth.token-created",
        &id,
        json!({"name": name}),
    );
    // The only time the token is shown (200, not 201: the body matters).
    Ok((
        StatusCode::OK,
        Json(json!({"id": id, "name": name, "token": token})),
    ))
}

async fn revoke_token(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    require(&app, &actor, Action::Account)?;
    match app.supervisor.store.delete_api_token(&id) {
        Ok(true) => {
            crate::audit::record(&actor.label(), "auth.token-revoked", &id, json!({}));
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err(deny(StatusCode::NOT_FOUND, "no such token")),
        Err(e) => Err(deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct ExpertBody {
    enabled: bool,
}

async fn set_expert(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    Json(b): Json<ExpertBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require(&app, &actor, Action::Account)?;
    app.auth
        .set_expert_mode(b.enabled)
        .map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))?;
    crate::audit::record(
        &actor.label(),
        if b.enabled {
            "expert.enabled"
        } else {
            "expert.disabled"
        },
        "",
        json!({}),
    );
    Ok(Json(json!({"expert_mode": b.enabled})))
}

/// For handlers gating expert surfaces.
pub fn require_expert(app: &App, actor: &Actor) -> Result<(), ApiError> {
    require(app, actor, Action::Expert)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashing_is_argon2id_and_salted() {
        let a = hash_password("correct horse").unwrap();
        let b = hash_password("correct horse").unwrap();
        assert!(a.starts_with("$argon2id$"), "{a}");
        assert_ne!(a, b, "salted");
        assert!(verify_password("correct horse", &a));
        assert!(!verify_password("wrong", &a));
        assert!(!verify_password("x", "not a hash"));
    }

    #[test]
    fn funnel() {
        for a in [
            Actor::Session,
            Actor::Bootstrap,
            Actor::Token("t".into()),
            Actor::LocalSetup,
        ] {
            assert!(can(&a, Action::Use, false));
        }
        assert!(!can(&Actor::Anonymous, Action::Use, true));
        assert!(can(&Actor::Anonymous, Action::Public, false));
        assert!(
            !can(&Actor::Token("t".into()), Action::Account, true),
            "tokens can't mint tokens"
        );
        assert!(
            !can(&Actor::Session, Action::Expert, false),
            "expert mode off"
        );
        assert!(can(&Actor::Session, Action::Expert, true));
        assert!(!can(&Actor::Token("t".into()), Action::Expert, true));
    }

    #[test]
    fn sessions_and_lockout() {
        let store = Store::memory().unwrap();
        let auth = Auth::new(store.clone(), false, false);
        let (token, csrf) = auth.start_session().unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            format!("a=b; {SESSION_COOKIE}={token}").parse().unwrap(),
        );
        assert_eq!(auth.session(&h).unwrap().csrf, csrf);
        h.insert(
            header::COOKIE,
            format!("{SESSION_COOKIE}=nope").parse().unwrap(),
        );
        assert!(auth.session(&h).is_none());

        let ip = Some("10.0.0.9".parse().unwrap());
        for _ in 0..FREE_ATTEMPTS - 1 {
            auth.login_failed(ip);
        }
        assert!(auth.locked(ip).is_none(), "free attempts");
        auth.login_failed(ip);
        assert!(auth.locked(ip).is_some(), "locked after the free attempts");
        assert!(
            auth.locked(Some("10.0.0.10".parse().unwrap())).is_none(),
            "per IP"
        );
        auth.login_ok(ip);
        assert!(auth.locked(ip).is_none());
    }

    #[test]
    fn cookie_attributes() {
        let a = Auth::new(Store::memory().unwrap(), true, false);
        let c = a.session_cookie("t", 10);
        let c = c.to_str().unwrap();
        for part in ["HttpOnly", "SameSite=Strict", "Secure", "Path=/"] {
            assert!(c.contains(part), "{c}");
        }
        let plain = Auth::new(Store::memory().unwrap(), false, false);
        assert!(
            !plain
                .session_cookie("t", 10)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
    }
}

/// The whole flow through the real router: setup, login, CSRF, tokens,
/// lockout, console tickets and expert gating.
#[cfg(test)]
mod wired {
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const HOST: &str = "127.0.0.1:7480";

    struct Resp {
        status: u16,
        cookie: Option<String>,
        body: Value,
    }

    async fn call(
        r: &axum::Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Resp {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", HOST);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = r.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let cookie = res
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_string());
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        Resp {
            status,
            cookie,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    fn router(tag: &str) -> (axum::Router, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("qui-auth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let scanner = crate::scan::Scanner::new(store.clone(), vec![], vec![]);
        let sv = crate::supervisor::Supervisor::new(store, scanner.clone(), &dir, dir.join("r"))
            .unwrap();
        let app = crate::api::App::new(scanner, sv, T.into());
        (crate::api::router(app, None), dir)
    }

    const BROWSER: [(&str, &str); 2] = [
        ("sec-fetch-site", "same-origin"),
        ("origin", "http://127.0.0.1:7480"),
    ];

    #[tokio::test]
    async fn setup_login_csrf_tokens_and_lockout() {
        let (r, dir) = router("flow");
        // Before setup: the local browser works as before.
        assert_eq!(
            call(&r, "GET", "/api/v1/vms", &BROWSER, None).await.status,
            200
        );
        let st = call(&r, "GET", "/api/v1/auth/state", &BROWSER, None)
            .await
            .body;
        assert_eq!(st["setup_complete"], false);

        // Short passwords are refused; setup logs the browser in.
        let bad = call(
            &r,
            "POST",
            "/api/v1/auth/setup",
            &BROWSER,
            Some(json!({"username": "admin", "password": "short"})),
        )
        .await;
        assert_eq!(bad.status, 422);
        let s = call(
            &r,
            "POST",
            "/api/v1/auth/setup",
            &BROWSER,
            Some(json!({"username": "admin", "password": "password-1"})),
        )
        .await;
        assert_eq!(s.status, 200, "{}", s.body);
        let cookie = s.cookie.expect("session cookie");
        let csrf = s.body["csrf"].as_str().unwrap().to_string();
        assert_eq!(
            call(
                &r,
                "POST",
                "/api/v1/auth/setup",
                &BROWSER,
                Some(json!({"username": "x", "password": "password-2"}))
            )
            .await
            .status,
            409,
            "setup happens once"
        );

        // After setup: no session, no access (but public endpoints answer).
        assert_eq!(
            call(&r, "GET", "/api/v1/vms", &BROWSER, None).await.status,
            401
        );
        assert_eq!(
            call(&r, "GET", "/api/v1/auth/state", &BROWSER, None)
                .await
                .status,
            200
        );
        let with_cookie = [BROWSER[0], BROWSER[1], ("cookie", cookie.as_str())];
        assert_eq!(
            call(&r, "GET", "/api/v1/vms", &with_cookie, None)
                .await
                .status,
            200
        );

        // Unsafe methods need the CSRF token too.
        let mk = Some(json!({"name": "ci"}));
        assert_eq!(
            call(&r, "POST", "/api/v1/auth/tokens", &with_cookie, mk.clone())
                .await
                .status,
            403
        );
        let with_csrf = [
            BROWSER[0],
            BROWSER[1],
            ("cookie", cookie.as_str()),
            ("x-csrf-token", csrf.as_str()),
        ];
        let tok = call(&r, "POST", "/api/v1/auth/tokens", &with_csrf, mk.clone()).await;
        assert_eq!(tok.status, 200, "{}", tok.body);
        let api_token = tok.body["token"].as_str().unwrap().to_string();

        // API tokens: usable, but can't manage accounts; revocable.
        let bearer = format!("Bearer {api_token}");
        assert_eq!(
            call(
                &r,
                "GET",
                "/api/v1/vms",
                &[("authorization", &bearer)],
                None
            )
            .await
            .status,
            200
        );
        assert_eq!(
            call(
                &r,
                "POST",
                "/api/v1/auth/tokens",
                &[("authorization", &bearer)],
                mk
            )
            .await
            .status,
            403
        );
        let list = call(&r, "GET", "/api/v1/auth/tokens", &with_csrf, None)
            .await
            .body;
        assert!(
            list.to_string().contains("\"ci\"") && !list.to_string().contains(&api_token),
            "{list}"
        );
        let id = tok.body["id"].as_str().unwrap();
        assert_eq!(
            call(
                &r,
                "DELETE",
                &format!("/api/v1/auth/tokens/{id}"),
                &with_csrf,
                None
            )
            .await
            .status,
            204
        );
        assert_eq!(
            call(
                &r,
                "GET",
                "/api/v1/vms",
                &[("authorization", &bearer)],
                None
            )
            .await
            .status,
            401
        );

        // Console tickets are minted only for authenticated actors.
        assert_eq!(
            call(
                &r,
                "POST",
                "/api/v1/vms/nope/consoles/vnc/ticket",
                &BROWSER,
                None
            )
            .await
            .status,
            401
        );

        // Login: wrong passwords lock the client out for a while.
        for _ in 0..super::FREE_ATTEMPTS {
            let w = call(
                &r,
                "POST",
                "/api/v1/auth/login",
                &BROWSER,
                Some(json!({"username": "admin", "password": "nope-nope"})),
            )
            .await;
            assert_eq!(w.status, 401);
        }
        let locked = call(
            &r,
            "POST",
            "/api/v1/auth/login",
            &BROWSER,
            Some(json!({"username": "admin", "password": "password-1"})),
        )
        .await;
        assert_eq!(
            locked.status, 429,
            "locked out even with the right password"
        );

        // Logout ends the session.
        assert_eq!(
            call(&r, "POST", "/api/v1/auth/logout", &with_csrf, None)
                .await
                .status,
            204
        );
        assert_eq!(
            call(&r, "GET", "/api/v1/vms", &with_cookie, None)
                .await
                .status,
            401
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn expert_surfaces_are_off_by_default() {
        let (r, dir) = router("expert");
        let boot = format!("Bearer {T}");
        let auth = [("authorization", boot.as_str())];
        // QMP console: refused while expert mode is off.
        assert_eq!(
            call(
                &r,
                "POST",
                "/api/v1/vms/x/qmp",
                &auth,
                Some(json!({"execute": "query-status"}))
            )
            .await
            .status,
            403
        );
        // Passthrough edits too (the spec is otherwise irrelevant here).
        let defaults = call(
            &r,
            "GET",
            "/api/v1/vms/defaults?arch=x86_64&name=pt",
            &auth,
            None,
        )
        .await;
        let mut spec = defaults.body["spec"].clone();
        if spec.is_object() {
            spec["passthrough"] = json!(["-global", "x.y=1"]);
            assert_eq!(
                call(
                    &r,
                    "POST",
                    "/api/v1/vms",
                    &auth,
                    Some(json!({"spec": spec}))
                )
                .await
                .status,
                403
            );
        }
        // Enabling it is an account action; then QMP reaches the VM check.
        assert_eq!(
            call(
                &r,
                "PUT",
                "/api/v1/settings/expert",
                &auth,
                Some(json!({"enabled": true}))
            )
            .await
            .status,
            200
        );
        let st = call(&r, "GET", "/api/v1/auth/state", &auth, None)
            .await
            .body;
        assert_eq!(st["expert_mode"], true);
        assert_eq!(
            call(
                &r,
                "POST",
                "/api/v1/vms/x/qmp",
                &auth,
                Some(json!({"execute": "query-status"}))
            )
            .await
            .status,
            409
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
