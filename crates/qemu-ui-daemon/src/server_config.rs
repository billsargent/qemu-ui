//! How this daemon is being served, set once at start-up and read anywhere
//! (e.g. session cookies get `Secure` only over TLS).

// The accessors are for consumers such as session cookies (M6-01).
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Serving HTTPS.
    pub tls: bool,
}

static CONFIG: OnceLock<ServerConfig> = OnceLock::new();

/// Record the server configuration. Later calls are ignored.
pub fn set(c: ServerConfig) {
    let _ = CONFIG.set(c);
}

pub fn get() -> Option<&'static ServerConfig> {
    CONFIG.get()
}

/// Are responses going out over TLS? (False before start-up and in tests.)
pub fn tls() -> bool {
    CONFIG.get().is_some_and(|c| c.tls)
}

/// TLS policy from the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TlsMode {
    /// HTTPS when listening beyond loopback; plain HTTP on loopback.
    Auto,
    On,
    /// Plain HTTP. Only allowed on loopback.
    Off,
}

/// Decide whether to serve TLS. Plain HTTP beyond loopback is refused.
pub fn want_tls(mode: TlsMode, listen: SocketAddr) -> Result<bool, String> {
    let loopback = listen.ip().is_loopback();
    match mode {
        TlsMode::On => Ok(true),
        TlsMode::Auto => Ok(!loopback),
        TlsMode::Off if loopback => Ok(false),
        TlsMode::Off => Err(format!(
            "refusing plain HTTP on {listen}: beyond loopback the daemon only serves HTTPS"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_policy() {
        let lo: SocketAddr = "127.0.0.1:7480".parse().unwrap();
        let lan: SocketAddr = "0.0.0.0:7480".parse().unwrap();
        assert_eq!(want_tls(TlsMode::Auto, lo), Ok(false));
        assert_eq!(want_tls(TlsMode::Auto, lan), Ok(true));
        assert_eq!(want_tls(TlsMode::On, lo), Ok(true));
        assert_eq!(want_tls(TlsMode::Off, lo), Ok(false));
        assert!(want_tls(TlsMode::Off, lan).is_err());
    }
}
