//! The helper's request loop: authenticate, validate, act, audit.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use qemu_ui_core::helper::{
    Envelope, HelperConfig, PROTOCOL_VERSION, Request, Response, check_export,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::exports::Exports;

pub struct Helper {
    pub config: HelperConfig,
    pub exports: Exports,
    /// Required in `hello` when set (Windows: no peer credentials).
    pub secret: Option<String>,
    /// Linux: the only uid allowed to connect.
    pub allowed_uid: Option<u32>,
    pub audit_log: PathBuf,
    audit_lock: Mutex<()>,
}

/// Who is on the other end, as far as the transport can tell.
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub uid: Option<u32>,
}

fn eq_ct(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Helper {
    pub fn new(
        config: HelperConfig,
        exports: Exports,
        secret: Option<String>,
        allowed_uid: Option<u32>,
        audit_log: PathBuf,
    ) -> Self {
        Helper {
            config,
            exports,
            secret,
            allowed_uid,
            audit_log,
            audit_lock: Mutex::new(()),
        }
    }

    fn audit(&self, peer: Peer, req: &str, outcome: &Result<String, String>) {
        let _g = self.audit_lock.lock().unwrap();
        let line = serde_json::json!({
            "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            "peer_uid": peer.uid,
            "request": req,
            "ok": outcome.is_ok(),
            "detail": match outcome { Ok(s) | Err(s) => s },
        });
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_log)
        {
            let _ = writeln!(f, "{line}");
        }
        tracing::info!(target: "audit", "{line}");
    }

    /// Serve one connection until it closes.
    pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: S,
        peer: Peer,
    ) -> Result<()> {
        if let (Some(want), Some(got)) = (self.allowed_uid, peer.uid)
            && want != got
        {
            self.audit(
                peer,
                "connect",
                &Err(format!("uid {got} is not the daemon user")),
            );
            return Ok(());
        }
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        let mut authed = false;
        while let Some(line) = lines.next_line().await? {
            let (id, resp) = match serde_json::from_str::<Envelope<Request>>(&line) {
                Err(e) => {
                    self.audit(peer, "malformed", &Err(e.to_string()));
                    (
                        0,
                        Response::Error {
                            message: format!("bad request: {e}"),
                        },
                    )
                }
                Ok(env) => (env.id, self.handle(env.body, peer, &mut authed).await),
            };
            let mut out = serde_json::to_vec(&Envelope { id, body: resp })?;
            out.push(b'\n');
            w.write_all(&out).await?;
            w.flush().await?;
        }
        Ok(())
    }

    async fn handle(&self, req: Request, peer: Peer, authed: &mut bool) -> Response {
        let name = serde_json::to_value(&req)
            .map(|mut v| {
                if let Some(o) = v.as_object_mut() {
                    o.remove("secret"); // never log secrets
                }
                v.to_string()
            })
            .unwrap_or_default();
        if !*authed && !matches!(req, Request::Hello { .. }) {
            self.audit(peer, &name, &Err("not authenticated".into()));
            return Response::Error {
                message: "send hello first".into(),
            };
        }
        let outcome: Result<Response, String> = match req {
            Request::Hello { version, secret } => {
                if version != PROTOCOL_VERSION {
                    Err(format!(
                        "protocol version {version} unsupported (helper speaks {PROTOCOL_VERSION})"
                    ))
                } else if let Some(want) = &self.secret
                    && !secret
                        .as_deref()
                        .is_some_and(|s| eq_ct(s.as_bytes(), want.as_bytes()))
                {
                    Err("authentication failed".into())
                } else {
                    *authed = true;
                    Ok(Response::Hello {
                        version: PROTOCOL_VERSION,
                    })
                }
            }
            Request::ListDisks => crate::disks::list()
                .await
                .map(|disks| Response::Disks { disks })
                .map_err(|e| format!("{e:#}")),
            Request::ExportDisk { disk_id, writable } => match crate::disks::list().await {
                Err(e) => Err(format!("{e:#}")),
                Ok(disks) => match disks.into_iter().find(|d| d.id == disk_id) {
                    None => Err(format!("no disk with id '{disk_id}'")),
                    Some(d) => {
                        match check_export(&d, &self.config.disks, writable, cfg!(windows)) {
                            Err(e) => Err(e),
                            Ok(()) => self
                                .exports
                                .export(&d, writable)
                                .await
                                .map(|export| Response::Exported { export })
                                .map_err(|e| format!("{e:#}")),
                        }
                    }
                },
            },
            Request::Unexport { export_id } => self
                .exports
                .unexport(&export_id)
                .map(|_| Response::Unexported)
                .map_err(|e| format!("{e:#}")),
            Request::ListExports => Ok(Response::Exports {
                exports: self.exports.list(),
            }),
        };
        let summary = outcome
            .as_ref()
            .map(|_| "ok".to_string())
            .map_err(Clone::clone);
        self.audit(peer, &name, &summary);
        outcome.unwrap_or_else(|message| Response::Error { message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    async fn session(
        h: std::sync::Arc<Helper>,
        peer: Peer,
        requests: &[&str],
    ) -> Vec<serde_json::Value> {
        let (a, b) = duplex(1 << 16);
        let server = tokio::spawn(async move { h.serve(b, peer).await });
        let (r, mut w) = tokio::io::split(a);
        let mut lines = BufReader::new(r).lines();
        let mut out = Vec::new();
        for req in requests {
            w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
            let line = lines.next_line().await.unwrap().unwrap_or_default();
            out.push(serde_json::from_str(&line).unwrap_or(serde_json::Value::Null));
        }
        // Both halves must go: a split duplex only closes when the read half
        // is dropped too, and the server waits for that EOF.
        drop(w);
        drop(lines);
        let _ = server.await;
        out
    }

    fn helper(secret: Option<&str>, uid: Option<u32>) -> (std::sync::Arc<Helper>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "quis-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exports =
            Exports::open(&dir.join("x"), std::path::Path::new("qemu-storage-daemon")).unwrap();
        let h = Helper::new(
            HelperConfig::default(),
            exports,
            secret.map(String::from),
            uid,
            dir.join("audit.log"),
        );
        (std::sync::Arc::new(h), dir)
    }

    #[tokio::test]
    async fn authentication_and_closed_request_set() {
        let (h, dir) = helper(Some("s3cret"), None);
        let r = session(
            h.clone(),
            Peer { uid: None },
            &[
                r#"{"id":1,"op":"list_exports"}"#,
                r#"{"id":2,"op":"hello","version":1,"secret":"wrong"}"#,
                r#"{"id":3,"op":"hello","version":1,"secret":"s3cret"}"#,
                r#"{"id":4,"op":"list_exports"}"#,
                r#"{"id":5,"op":"run_command","cmd":"sh"}"#,
                r#"{"id":6,"op":"export_disk","disk_id":"definitely-not-a-disk","writable":false}"#,
            ],
        )
        .await;
        assert_eq!(r[0]["result"], "error", "requests before hello are refused");
        assert_eq!(r[1]["message"], "authentication failed");
        assert_eq!(
            (r[2]["result"].as_str(), r[2]["id"].as_u64()),
            (Some("hello"), Some(3))
        );
        assert_eq!(r[3]["result"], "exports");
        assert!(
            r[4]["message"].as_str().unwrap().contains("bad request"),
            "no such operation"
        );
        assert_eq!(r[5]["result"], "error");
        let audit = std::fs::read_to_string(&h.audit_log).unwrap();
        assert!(audit.lines().count() >= 6);
        assert!(!audit.contains("s3cret"), "secrets are never logged");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn wrong_uid_is_disconnected() {
        let (h, dir) = helper(None, Some(1000));
        let r = session(
            h,
            Peer { uid: Some(0) },
            &[r#"{"id":1,"op":"hello","version":1}"#],
        )
        .await;
        assert!(r[0].is_null(), "connection closed without a reply");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lists_real_disks_without_privileges() {
        let (h, dir) = helper(None, None);
        let r = session(
            h,
            Peer { uid: None },
            &[
                r#"{"id":1,"op":"hello","version":1}"#,
                r#"{"id":2,"op":"list_disks"}"#,
            ],
        )
        .await;
        // Listing works unprivileged on Windows (Get-Disk) and Linux (lsblk);
        // CI containers may lack disks, so only the shape is asserted.
        assert!(
            matches!(r[1]["result"].as_str(), Some("disks") | Some("error")),
            "{}",
            r[1]
        );
        if r[1]["result"] == "disks" {
            for d in r[1]["disks"].as_array().unwrap() {
                assert!(!d["id"].as_str().unwrap().is_empty());
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
