//! TLS (plan.md §13, M6-02): HTTPS by default whenever the daemon listens
//! beyond loopback; plain HTTP stays the loopback default.
//!
//! * **Generated:** on first use the daemon creates its own CA and a server
//!   certificate signed by it, covering `localhost`, the loopback addresses,
//!   the host name and the addresses it listens on. Users trust the CA once
//!   (the trust page serves it with its fingerprint); the server certificate
//!   is reissued from the same CA when names change or expiry nears, so that
//!   trust survives. Keys are written owner-only on Unix.
//! * **Provided (BYO):** `--tls-cert`/`--tls-key` PEM files, validated at
//!   start (parse, key matches certificate, not expired) and reloadable
//!   (`POST /tls/reload`, or SIGHUP on Unix) without dropping connections.
//!
//! rustls with the `ring` provider: pure Rust, no OpenSSL.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::security::TlsInfo;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, App};

/// Reissue the generated server certificate this long before it expires.
const RENEW_BEFORE_DAYS: i64 = 30;
/// Apple platforms reject server certificates valid for more than 825 days,
/// even under a user-trusted root; stay well inside that.
const LEAF_DAYS: i64 = 397;
const CA_YEARS: i64 = 10;

pub const HSTS: &str = "max-age=31536000";

/// Where the serving certificate comes from.
#[derive(Debug, Clone)]
pub enum Source {
    Generated { dir: PathBuf },
    Provided { cert: PathBuf, key: PathBuf },
}

/// Hands rustls the current certificate; swapped on reload.
#[derive(Debug)]
pub struct Resolver {
    current: RwLock<Arc<CertifiedKey>>,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().unwrap().clone())
    }
}

/// The live TLS setup.
pub struct Tls {
    source: Source,
    names: Vec<String>,
    resolver: Arc<Resolver>,
    info: RwLock<TlsInfo>,
    pub config: Arc<rustls::ServerConfig>,
}

static TLS: OnceLock<Arc<Tls>> = OnceLock::new();

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Names and addresses the generated certificate should cover for a
/// listener on `listen`.
pub fn default_names(listen: SocketAddr) -> Vec<String> {
    let mut set: BTreeSet<String> = ["localhost", "127.0.0.1", "::1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Ok(h) = sysinfo::System::host_name()
        && !h.is_empty()
    {
        set.insert(h.to_ascii_lowercase());
    }
    let ip = listen.ip();
    if ip.is_unspecified() {
        let nets = sysinfo::Networks::new_with_refreshed_list().ok();
        for data in nets.iter().flat_map(|n| n.values()) {
            for n in data.ip_networks() {
                let a = n.addr;
                let v4_ok = matches!(a, IpAddr::V4(v) if !v.is_link_local());
                let v6_ok = matches!(a, IpAddr::V6(v) if !v.is_unicast_link_local());
                if !a.is_unspecified() && (v4_ok || v6_ok) {
                    set.insert(a.to_string());
                }
            }
        }
    } else {
        set.insert(ip.to_string());
    }
    set.into_iter().collect()
}

#[cfg(unix)]
fn owner_only(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", p.display()))
}

#[cfg(not(unix))]
fn owner_only(_p: &Path) -> Result<()> {
    // Windows: the data directory is per-user; files inherit its ACL.
    Ok(())
}

fn write_secret(p: &Path, contents: &str) -> Result<()> {
    std::fs::write(p, contents).with_context(|| format!("writing {}", p.display()))?;
    owner_only(p)
}

fn days(n: i64) -> time::Duration {
    time::Duration::days(n)
}

/// Load the generated CA, or create it.
fn load_or_create_ca(dir: &Path) -> Result<(String, rcgen::KeyPair)> {
    let (cert_p, key_p) = (dir.join("ca.pem"), dir.join("ca.key"));
    if let (Ok(cert), Ok(key)) = (
        std::fs::read_to_string(&cert_p),
        std::fs::read_to_string(&key_p),
    ) {
        let kp = rcgen::KeyPair::from_pem(&key).context("reading the generated CA key")?;
        return Ok((cert, kp));
    }
    let kp = rcgen::KeyPair::generate().context("generating the CA key")?;
    let mut p = rcgen::CertificateParams::new(Vec::<String>::new())?;
    let mut dn = rcgen::DistinguishedName::new();
    let tag: String = (0..4)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    dn.push(rcgen::DnType::CommonName, format!("qemu-ui local CA {tag}"));
    dn.push(rcgen::DnType::OrganizationName, "qemu-ui");
    p.distinguished_name = dn;
    p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    p.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - days(1);
    p.not_after = now + days(365 * CA_YEARS);
    let cert = p.self_signed(&kp)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    write_secret(&key_p, &kp.serialize_pem())?;
    std::fs::write(&cert_p, cert.pem()).with_context(|| format!("writing {}", cert_p.display()))?;
    tracing::info!("generated a local certificate authority for HTTPS");
    crate::audit::record("daemon", "tls.ca.generate", "tls", serde_json::json!({}));
    Ok((cert.pem(), kp))
}

/// What a certificate covers and until when.
pub struct CertFacts {
    pub names: Vec<String>,
    pub not_after: u64,
    pub not_before: u64,
    pub is_ca: bool,
}

pub fn cert_facts(der: &[u8]) -> Result<CertFacts> {
    let (_, x) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| anyhow!("not a valid X.509 certificate: {e}"))?;
    let mut names = Vec::new();
    if let Ok(Some(san)) = x.subject_alternative_name() {
        for n in &san.value.general_names {
            match n {
                x509_parser::extensions::GeneralName::DNSName(d) => names.push(d.to_string()),
                x509_parser::extensions::GeneralName::IPAddress(b) => match b.len() {
                    4 => names.push(Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string()),
                    16 => {
                        let a: [u8; 16] = (*b).try_into().unwrap_or([0; 16]);
                        names.push(Ipv6Addr::from(a).to_string());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    let v = x.validity();
    Ok(CertFacts {
        names,
        not_after: v.not_after.timestamp().max(0) as u64,
        not_before: v.not_before.timestamp().max(0) as u64,
        is_ca: x.is_ca(),
    })
}

/// The generated server certificate for `names`, reissued when needed.
/// Returns (chain PEM [leaf, CA], leaf key PEM).
fn ensure_generated(dir: &Path, names: &[String]) -> Result<(String, String)> {
    let (ca_pem, ca_key) = load_or_create_ca(dir)?;
    let (cert_p, key_p) = (dir.join("server.pem"), dir.join("server.key"));
    if let (Ok(cert), Ok(key)) = (
        std::fs::read_to_string(&cert_p),
        std::fs::read_to_string(&key_p),
    ) && let Ok(der) = CertificateDer::from_pem_slice(cert.as_bytes())
        && let Ok(f) = cert_facts(&der)
    {
        let have: BTreeSet<&String> = f.names.iter().collect();
        let covers = names.iter().all(|n| have.contains(n));
        let fresh = f.not_after as i64 > now_secs() as i64 + RENEW_BEFORE_DAYS * 86_400;
        if covers && fresh {
            return Ok((format!("{cert}{ca_pem}"), key));
        }
    }
    let issuer =
        rcgen::Issuer::from_ca_cert_pem(&ca_pem, &ca_key).context("loading the generated CA")?;
    let kp = rcgen::KeyPair::generate()?;
    let mut p = rcgen::CertificateParams::new(Vec::<String>::new())?;
    for n in names {
        p.subject_alt_names.push(match n.parse::<IpAddr>() {
            Ok(ip) => rcgen::SanType::IpAddress(ip),
            Err(_) => rcgen::SanType::DnsName(n.clone().try_into()?),
        });
    }
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "qemu-ui");
    p.distinguished_name = dn;
    p.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyEncipherment,
    ];
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    p.use_authority_key_identifier_extension = true;
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - days(1);
    p.not_after = now + days(LEAF_DAYS);
    let cert = p.signed_by(&kp, &issuer)?;
    write_secret(&key_p, &kp.serialize_pem())?;
    std::fs::write(&cert_p, cert.pem()).with_context(|| format!("writing {}", cert_p.display()))?;
    tracing::info!("issued a server certificate for {}", names.join(", "));
    crate::audit::record(
        "daemon",
        "tls.cert.issue",
        "tls",
        serde_json::json!({ "names": names }),
    );
    Ok((format!("{}{ca_pem}", cert.pem()), kp.serialize_pem()))
}

/// Parse and check a PEM chain + key; the key must match the leaf.
pub fn certified_key(chain_pem: &[u8], key_pem: &[u8]) -> Result<Arc<CertifiedKey>> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("reading the certificate PEM: {e}"))?;
    if chain.is_empty() {
        bail!("the certificate file holds no certificates");
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| anyhow!("reading the private key PEM: {e}"))?;
    let signing = provider()
        .key_provider
        .load_private_key(key)
        .map_err(|e| anyhow!("unsupported private key: {e}"))?;
    let ck = CertifiedKey::new(chain, signing);
    ck.keys_match()
        .map_err(|e| anyhow!("the private key does not belong to the certificate: {e}"))?;
    let f = cert_facts(&ck.cert[0])?;
    let now = now_secs();
    if f.not_after <= now {
        bail!("the certificate expired");
    }
    if f.not_before > now + 300 {
        bail!("the certificate is not valid yet");
    }
    if f.is_ca {
        bail!("the first certificate is a CA certificate; put the server certificate first");
    }
    Ok(Arc::new(ck))
}

fn load(source: &Source, names: &[String]) -> Result<(Arc<CertifiedKey>, TlsInfo)> {
    match source {
        Source::Generated { dir } => {
            let (chain, key) = ensure_generated(dir, names)?;
            let ck = certified_key(chain.as_bytes(), key.as_bytes())?;
            let ca = ck
                .cert
                .get(1)
                .cloned()
                .context("generated chain lacks its CA")?;
            let f = cert_facts(&ck.cert[0])?;
            Ok((
                ck,
                TlsInfo {
                    enabled: true,
                    source: Some("generated".into()),
                    fingerprint_sha256: Some(fingerprint(&ca)),
                    names: f.names,
                    not_after: Some(f.not_after),
                    ca_download: true,
                },
            ))
        }
        Source::Provided { cert, key } => {
            let c = std::fs::read(cert).with_context(|| format!("reading {}", cert.display()))?;
            let k = std::fs::read(key).with_context(|| format!("reading {}", key.display()))?;
            let ck = certified_key(&c, &k)
                .with_context(|| format!("TLS certificate {}", cert.display()))?;
            let f = cert_facts(&ck.cert[0])?;
            Ok((
                ck.clone(),
                TlsInfo {
                    enabled: true,
                    source: Some("provided".into()),
                    fingerprint_sha256: Some(fingerprint(&ck.cert[0])),
                    names: f.names,
                    not_after: Some(f.not_after),
                    ca_download: false,
                },
            ))
        }
    }
}

impl Tls {
    pub fn new(source: Source, names: Vec<String>) -> Result<Arc<Tls>> {
        let (ck, info) = load(&source, &names)?;
        let resolver = Arc::new(Resolver {
            current: RwLock::new(ck),
        });
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| anyhow!("TLS setup: {e}"))?
            .with_no_client_auth()
            .with_cert_resolver(resolver.clone());
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(Tls {
            source,
            names,
            resolver,
            info: RwLock::new(info),
            config: Arc::new(config),
        }))
    }

    /// Re-read the certificate (BYO files changed, or a generated one is
    /// due). Existing connections keep their session; new ones get the new
    /// certificate. On error the old certificate stays in service.
    pub fn reload(&self) -> Result<TlsInfo> {
        let (ck, info) = load(&self.source, &self.names)?;
        *self.resolver.current.write().unwrap() = ck;
        *self.info.write().unwrap() = info.clone();
        Ok(info)
    }

    pub fn info(&self) -> TlsInfo {
        self.info.read().unwrap().clone()
    }

    pub fn ca_pem(&self) -> Option<String> {
        match &self.source {
            Source::Generated { dir } => std::fs::read_to_string(dir.join("ca.pem")).ok(),
            Source::Provided { .. } => None,
        }
    }

    /// Make this the process's TLS setup (for the API endpoints).
    pub fn install(self: &Arc<Self>) {
        let _ = TLS.set(self.clone());
    }
}

// ------------------------------------------------------------------ serve

/// Serve `app` over HTTPS until `shutdown` resolves. Each request carries
/// the peer address as `ConnectInfo`, like the plain-HTTP path.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    tls: Arc<Tls>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use hyper_util::service::TowerToHyperService;
    use tower::ServiceExt;

    let acceptor = tokio_rustls::TlsAcceptor::from(tls.config.clone());
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = &mut shutdown => return Ok(()),
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                acceptor.accept(stream),
            )
            .await
            {
                Ok(Ok(s)) => s,
                // Plain HTTP to the HTTPS port, scanners, aborted handshakes.
                Ok(Err(e)) => {
                    tracing::debug!(%peer, "TLS handshake failed: {e}");
                    return;
                }
                Err(_) => return,
            };
            let svc = tower::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(ConnectInfo(peer));
                app.clone().oneshot(req.map(axum::body::Body::new))
            });
            if let Err(e) = Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls), TowerToHyperService::new(svc))
                .await
            {
                tracing::debug!(%peer, "connection: {e}");
            }
        });
    }
}

/// `Strict-Transport-Security` on every response served over TLS.
pub async fn hsts(req: Request, next: axum::middleware::Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static(HSTS),
    );
    res
}

// -------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/tls", get(info))
        .route("/tls/ca.pem", get(ca))
        .route("/tls/reload", post(reload))
}

async fn info() -> Json<TlsInfo> {
    Json(TLS.get().map(|t| t.info()).unwrap_or(TlsInfo {
        enabled: false,
        source: None,
        fingerprint_sha256: None,
        names: vec![],
        not_after: None,
        ca_download: false,
    }))
}

async fn ca() -> Result<Response, ApiError> {
    let pem = TLS.get().and_then(|t| t.ca_pem()).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "no generated certificate authority (HTTPS is off, or a provided certificate is in use)"
                .into(),
        )
    })?;
    let mut res = pem.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-pem-file"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"qemu-ui-ca.pem\""),
    );
    Ok(res)
}

async fn reload() -> Result<Json<TlsInfo>, ApiError> {
    let t = TLS
        .get()
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "HTTPS is not enabled".into()))?;
    t.reload()
        .map(Json)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

/// Reload the certificate on SIGHUP (Unix).
#[cfg(unix)]
pub fn reload_on_sighup(tls: Arc<Tls>) {
    tokio::spawn(async move {
        let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            return;
        };
        while s.recv().await.is_some() {
            match tls.reload() {
                Ok(_) => tracing::info!("TLS certificate reloaded"),
                Err(e) => tracing::error!("TLS reload failed, keeping the old certificate: {e:#}"),
            }
        }
    });
}

#[cfg(not(unix))]
pub fn reload_on_sighup(_tls: Arc<Tls>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "qui-tls-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names() -> Vec<String> {
        vec![
            "localhost".into(),
            "127.0.0.1".into(),
            "::1".into(),
            "vmhost.example".into(),
        ]
    }

    #[test]
    fn generated_certificate_covers_names_and_is_reused() {
        let dir = tmp("gen");
        let t = Tls::new(Source::Generated { dir: dir.clone() }, names()).unwrap();
        let info = t.info();
        assert_eq!(info.source.as_deref(), Some("generated"));
        for n in names() {
            assert!(info.names.contains(&n), "{n} in {:?}", info.names);
        }
        let validity = info.not_after.unwrap() - now_secs();
        assert!(
            validity > 390 * 86_400 && validity < 400 * 86_400,
            "{validity}"
        );
        let ca = t.ca_pem().unwrap();
        let ca_der = CertificateDer::from_pem_slice(ca.as_bytes()).unwrap();
        assert!(cert_facts(&ca_der).unwrap().is_ca);
        assert_eq!(info.fingerprint_sha256.unwrap(), fingerprint(&ca_der));

        // Same names: nothing reissued. New name: new leaf, same CA.
        let leaf = std::fs::read_to_string(dir.join("server.pem")).unwrap();
        let again = Tls::new(Source::Generated { dir: dir.clone() }, names()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("server.pem")).unwrap(),
            leaf
        );
        let mut more = names();
        more.push("10.9.8.7".into());
        let wider = Tls::new(Source::Generated { dir: dir.clone() }, more).unwrap();
        assert_ne!(
            std::fs::read_to_string(dir.join("server.pem")).unwrap(),
            leaf
        );
        assert!(wider.info().names.contains(&"10.9.8.7".to_string()));
        assert_eq!(wider.ca_pem().unwrap(), ca, "trust survives reissue");
        assert_eq!(
            again.info().fingerprint_sha256,
            wider.info().fingerprint_sha256
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for k in ["ca.key", "server.key"] {
                let mode = std::fs::metadata(dir.join(k)).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{k}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A BYO pair made with rcgen (self-signed leaf).
    fn byo(dir: &Path, not_after_days: i64) -> (PathBuf, PathBuf) {
        let kp = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(vec!["vmhost.example".to_string()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - days(2);
        p.not_after = now + days(not_after_days);
        let c = p.self_signed(&kp).unwrap();
        let (cp, kp_path) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cp, c.pem()).unwrap();
        std::fs::write(&kp_path, kp.serialize_pem()).unwrap();
        (cp, kp_path)
    }

    #[test]
    fn provided_certificates_are_validated() {
        let dir = tmp("byo");
        let (cert, key) = byo(&dir, 30);
        let t = Tls::new(
            Source::Provided {
                cert: cert.clone(),
                key: key.clone(),
            },
            vec![],
        )
        .unwrap();
        assert_eq!(t.info().source.as_deref(), Some("provided"));
        assert!(t.ca_pem().is_none());

        // Mismatched key.
        let other = tmp("byo2");
        let (_, wrong_key) = byo(&other, 30);
        let e = Tls::new(
            Source::Provided {
                cert: cert.clone(),
                key: wrong_key,
            },
            vec![],
        )
        .err()
        .unwrap();
        assert!(format!("{e:#}").contains("does not belong"), "{e:#}");
        // Expired.
        let (old, old_key) = byo(&other, -1);
        let e = Tls::new(
            Source::Provided {
                cert: old,
                key: old_key,
            },
            vec![],
        )
        .err()
        .unwrap();
        assert!(format!("{e:#}").contains("expired"), "{e:#}");
        // Garbage.
        std::fs::write(other.join("junk.pem"), "not a cert").unwrap();
        assert!(
            Tls::new(
                Source::Provided {
                    cert: other.join("junk.pem"),
                    key
                },
                vec![]
            )
            .is_err()
        );

        // Reload swaps in a renewed pair; a broken file keeps the old one.
        let before = t.info().fingerprint_sha256;
        byo(&dir, 60);
        let after = t.reload().unwrap().fingerprint_sha256;
        assert_ne!(before, after);
        std::fs::write(&cert, "broken").unwrap();
        assert!(t.reload().is_err());
        assert_eq!(t.info().fingerprint_sha256, after);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// HTTPS end to end: a rustls client that trusts only the generated CA
    /// completes the handshake and sees HSTS; plain HTTP has no HSTS.
    #[test]
    fn https_handshake_and_hsts() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tmp("hs");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tls = Tls::new(Source::Generated { dir: dir.clone() }, names()).unwrap();
            let app = Router::new()
                .route("/hello", get(|| async { "hi" }))
                .layer(axum::middleware::from_fn(hsts));
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(serve(l, app, tls.clone(), async {
                let _ = stop_rx.await;
            }));

            let mut roots = rustls::RootCertStore::empty();
            let ca = tls.ca_pem().unwrap();
            roots
                .add(CertificateDer::from_pem_slice(ca.as_bytes()).unwrap())
                .unwrap();
            let cfg = rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
            for name in ["localhost", "127.0.0.1"] {
                let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                let sn = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
                let mut s = connector.connect(sn, tcp).await.unwrap();
                s.write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                assert!(out.starts_with("HTTP/1.1 200"), "{out}");
                assert!(
                    out.to_ascii_lowercase()
                        .contains("strict-transport-security: max-age="),
                    "{out}"
                );
                assert!(out.ends_with("hi"), "{out}");
            }
            // A name the certificate doesn't cover fails verification.
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let sn = rustls::pki_types::ServerName::try_from("other.example".to_string()).unwrap();
            assert!(connector.connect(sn, tcp).await.is_err());
            let _ = stop_tx.send(());
            server.await.unwrap().unwrap();
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
