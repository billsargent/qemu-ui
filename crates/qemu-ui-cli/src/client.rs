//! HTTP client for the daemon's API: bearer-token auth, and TLS that trusts
//! the daemon's generated CA (M6-02) as well as the usual public roots.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use ureq::Agent;
use ureq::tls::{Certificate, RootCerts, TlsConfig};

pub struct Client {
    base: String,
    token: String,
    agent: Agent,
}

/// Which roots to trust for `https://` URLs.
pub enum Trust<'a> {
    /// The platform's usual roots (a certificate you installed yourself).
    Default,
    /// A PEM file: the daemon's CA (`/api/v1/tls/ca.pem`, or
    /// `<data-dir>/tls/ca.pem`).
    Ca(&'a Path),
}

impl Client {
    pub fn new(url: &str, token: String, trust: Trust) -> Result<Client> {
        let mut cfg = Agent::config_builder().http_status_as_error(false);
        if let Trust::Ca(path) = trust {
            let pem = std::fs::read(path)
                .with_context(|| format!("reading the CA certificate {}", path.display()))?;
            let cert = Certificate::from_pem(&pem)
                .with_context(|| format!("{} is not a PEM certificate", path.display()))?;
            cfg = cfg.tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::new_with_certs(&[cert]))
                    .build(),
            );
        }
        Ok(Client {
            base: format!("{}/api/v1", url.trim_end_matches('/')),
            token,
            agent: cfg.build().new_agent(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn finish(
        &self,
        url: &str,
        res: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Value> {
        let mut res = res.map_err(|e| match e {
            ureq::Error::Tls(_) | ureq::Error::Rustls(_) => anyhow::anyhow!(
                "TLS to {url} failed: {e}. If the daemon uses its own CA, pass --ca-cert (download it from /api/v1/tls/ca.pem)"
            ),
            e => anyhow::anyhow!("contacting the daemon at {url}: {e}; is qemu-uid running?"),
        })?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let body = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        check(status, body)
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        let url = self.url(path);
        let r = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth())
            .call();
        self.finish(&url, r)
    }

    pub fn post(&self, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = self.url(path);
        let req = self.agent.post(&url).header("Authorization", &self.auth());
        let r = match body {
            Some(b) => req.send_json(b),
            None => req.send_empty(),
        };
        self.finish(&url, r)
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value> {
        let url = self.url(path);
        let r = self
            .agent
            .put(&url)
            .header("Authorization", &self.auth())
            .send_json(body);
        self.finish(&url, r)
    }

    pub fn delete(&self, path: &str) -> Result<Value> {
        let url = self.url(path);
        let r = self
            .agent
            .delete(&url)
            .header("Authorization", &self.auth())
            .call();
        self.finish(&url, r)
    }

    /// Resolve a VM name to its id (ids pass through).
    pub fn vm_id(&self, vm: &str) -> Result<String> {
        let vms = self.get("/vms")?;
        vms.as_array()
            .into_iter()
            .flatten()
            .find(|v| v["id"] == vm || v["name"] == vm)
            .and_then(|v| v["id"].as_str().map(String::from))
            .with_context(|| format!("no VM named '{vm}'"))
    }

    /// Resolve a network name to its id (ids pass through).
    pub fn network_id(&self, net: &str) -> Result<String> {
        let nets = self.get("/networks")?;
        nets.as_array()
            .into_iter()
            .flatten()
            .find(|n| n["id"] == net || n["name"] == net)
            .and_then(|n| n["id"].as_str().map(String::from))
            .with_context(|| format!("no network named '{net}'"))
    }
}

pub fn check(status: u16, body: Value) -> Result<Value> {
    if status < 400 {
        return Ok(body);
    }
    let msg = body["error"]
        .as_str()
        .or(body.as_str())
        .unwrap_or("request failed")
        .to_string();
    let diags: Vec<String> = body["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            format!(
                "  {}: {}",
                d["field"].as_str().unwrap_or(""),
                d["message"].as_str().unwrap_or("")
            )
        })
        .collect();
    bail!(
        "{msg} (HTTP {status}){}{}",
        if diags.is_empty() { "" } else { "\n" },
        diags.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn errors_carry_message_and_diagnostics() {
        assert_eq!(check(200, json!(1)).unwrap(), json!(1));
        let e = check(
            422,
            json!({"error": "bad", "diagnostics": [{"field": "memory", "message": "too small"}]}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("bad (HTTP 422)") && e.contains("memory: too small"),
            "{e}"
        );
        let e = check(404, json!("plain text")).unwrap_err().to_string();
        assert!(e.contains("plain text"), "{e}");
    }
}
