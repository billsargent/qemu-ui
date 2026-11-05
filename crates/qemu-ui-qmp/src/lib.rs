//! Async QMP (QEMU Machine Protocol) client.
//!
//! QMP is newline-delimited JSON. After connecting, the server sends a
//! greeting; the client must send `qmp_capabilities` before any other command.
//! Replies are correlated with requests by the `id` field we attach, so many
//! commands may be in flight at once; asynchronous events are delivered on a
//! broadcast channel and never block command replies.
//!
//! The client is transport-agnostic: give it any `AsyncRead` + `AsyncWrite`
//! pair. Helpers exist for TCP, unix sockets and a child's stdio.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, oneshot};

/// Default time to wait for a command reply.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum QmpError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// QEMU rejected the command, e.g. class `GenericError` or `CommandNotFound`.
    #[error("{class}: {desc}")]
    Command { class: String, desc: String },
    #[error("connection closed")]
    Closed,
    #[error("timed out waiting for reply to {0}")]
    Timeout(String),
}

impl QmpError {
    /// True when QEMU says the command does not exist in this version/build.
    pub fn is_command_not_found(&self) -> bool {
        matches!(self, QmpError::Command { class, .. } if class == "CommandNotFound")
    }
}

pub type Result<T> = std::result::Result<T, QmpError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Greeting {
    pub version: Version,
    pub package: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub event: String,
    #[serde(default)]
    pub data: Value,
    /// Microseconds since the Unix epoch, from QEMU's timestamp.
    pub timestamp_us: u64,
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// A connected, negotiated QMP session. Cheap to clone.
#[derive(Clone)]
pub struct Qmp {
    inner: Arc<Inner>,
}

struct Inner {
    writer: Mutex<Writer>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<Event>,
    greeting: Greeting,
    timeout: Duration,
}

impl Qmp {
    /// Read the greeting, start the reader task, and negotiate capabilities.
    pub async fn connect<R, W>(reader: R, writer: W) -> Result<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mut lines = BufReader::new(reader).lines();
        let greeting = loop {
            let line = lines.next_line().await?.ok_or(QmpError::Closed)?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            break parse_greeting(line)?;
        };

        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(1024);
        let inner = Arc::new(Inner {
            writer: Mutex::new(Box::new(writer)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            events: events.clone(),
            greeting,
            timeout: DEFAULT_TIMEOUT,
        });

        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                dispatch(&line, &pending, &events);
            }
            // Fail everything still waiting.
            for (_, tx) in pending.lock().unwrap().drain() {
                let _ = tx.send(Err(QmpError::Closed));
            }
        });

        let qmp = Qmp { inner };
        qmp.execute("qmp_capabilities", None).await?;
        Ok(qmp)
    }

    /// Connect over TCP (loopback only, by policy of the caller).
    pub async fn connect_tcp(addr: impl tokio::net::ToSocketAddrs) -> Result<Self> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (r, w) = stream.into_split();
        Self::connect(r, w).await
    }

    /// Connect over a unix socket.
    #[cfg(unix)]
    pub async fn connect_unix(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (r, w) = stream.into_split();
        Self::connect(r, w).await
    }

    /// Use a child started with `-qmp stdio`. Takes its stdin and stdout.
    pub async fn from_child(child: &mut tokio::process::Child) -> Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| QmpError::Protocol("child stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| QmpError::Protocol("child stdout not piped".into()))?;
        Self::connect(stdout, stdin).await
    }

    pub fn greeting(&self) -> &Greeting {
        &self.inner.greeting
    }

    /// Subscribe to asynchronous events from now on.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Execute a command and return its `return` value.
    pub async fn execute(&self, command: &str, arguments: Option<Value>) -> Result<Value> {
        self.execute_with_timeout(command, arguments, self.inner.timeout)
            .await
    }

    pub async fn execute_with_timeout(
        &self,
        command: &str,
        arguments: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = Map::new();
        msg.insert("execute".into(), Value::String(command.into()));
        if let Some(args) = arguments {
            msg.insert("arguments".into(), args);
        }
        msg.insert("id".into(), json!(id));
        let mut line = serde_json::to_vec(&msg).expect("serialisable");
        line.push(b'\n');

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        {
            let mut w = self.inner.writer.lock().await;
            let res = async {
                w.write_all(&line).await?;
                w.flush().await
            }
            .await;
            if let Err(e) = res {
                self.inner.pending.lock().unwrap().remove(&id);
                return Err(e.into());
            }
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(QmpError::Closed),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(QmpError::Timeout(command.into()))
            }
        }
    }

    /// Execute and deserialise the reply.
    pub async fn call<T: serde::de::DeserializeOwned>(
        &self,
        command: &str,
        arguments: Option<Value>,
    ) -> Result<T> {
        let v = self.execute(command, arguments).await?;
        serde_json::from_value(v).map_err(|e| QmpError::Protocol(format!("{command}: {e}")))
    }
}

fn parse_greeting(line: &str) -> Result<Greeting> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| QmpError::Protocol(format!("bad greeting: {e}: {line}")))?;
    let qmp = v
        .get("QMP")
        .ok_or_else(|| QmpError::Protocol(format!("not a QMP greeting: {line}")))?;
    let ver = &qmp["version"];
    let version: Version = serde_json::from_value(ver["qemu"].clone())
        .map_err(|e| QmpError::Protocol(format!("bad greeting version: {e}")))?;
    Ok(Greeting {
        version,
        package: ver["package"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string(),
        capabilities: qmp["capabilities"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn dispatch(line: &str, pending: &Pending, events: &broadcast::Sender<Event>) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        tracing::warn!(%line, "unparseable QMP line");
        return;
    };
    if let Some(name) = v.get("event").and_then(Value::as_str) {
        let ts = &v["timestamp"];
        let timestamp_us = ts["seconds"].as_u64().unwrap_or(0) * 1_000_000
            + ts["microseconds"].as_u64().unwrap_or(0);
        let _ = events.send(Event {
            event: name.to_string(),
            data: v.get("data").cloned().unwrap_or(Value::Null),
            timestamp_us,
        });
        return;
    }
    let Some(id) = v.get("id").and_then(Value::as_u64) else {
        tracing::warn!(%line, "QMP reply without our id");
        return;
    };
    let Some(tx) = pending.lock().unwrap().remove(&id) else {
        return; // timed out earlier
    };
    let result = if let Some(ret) = v.get("return") {
        Ok(ret.clone())
    } else if let Some(err) = v.get("error") {
        Err(QmpError::Command {
            class: err["class"].as_str().unwrap_or("GenericError").into(),
            desc: err["desc"].as_str().unwrap_or_default().into(),
        })
    } else {
        Err(QmpError::Protocol(format!("unexpected reply: {line}")))
    };
    let _ = tx.send(result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    const GREETING: &str = r#"{"QMP": {"version": {"qemu": {"micro": 0, "minor": 1, "major": 11}, "package": "v11.1.0"}, "capabilities": ["oob"]}}"#;

    /// A scripted fake QEMU: answers by command name, emits one event first.
    async fn fake_server(stream: tokio::io::DuplexStream) {
        let (r, mut w) = tokio::io::split(stream);
        w.write_all(format!("{GREETING}\r\n").as_bytes())
            .await
            .unwrap();
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let v: Value = serde_json::from_str(&line).unwrap();
            let id = v["id"].clone();
            let reply = match v["execute"].as_str().unwrap() {
                "qmp_capabilities" => json!({"return": {}, "id": id}),
                "query-status" => {
                    let ev = json!({"event": "STOP", "data": {}, "timestamp": {"seconds": 2, "microseconds": 5}});
                    w.write_all(format!("{ev}\n").as_bytes()).await.unwrap();
                    json!({"return": {"status": "paused", "running": false}, "id": id})
                }
                "echo" => json!({"return": v["arguments"].clone(), "id": id}),
                _ => json!({"error": {"class": "CommandNotFound", "desc": "nope"}, "id": id}),
            };
            w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        }
    }

    async fn client() -> Qmp {
        let (a, b) = duplex(4096);
        tokio::spawn(fake_server(b));
        let (r, w) = tokio::io::split(a);
        Qmp::connect(r, w).await.unwrap()
    }

    #[tokio::test]
    async fn negotiates_and_executes() {
        let q = client().await;
        assert_eq!(q.greeting().version.major, 11);
        assert_eq!(q.greeting().capabilities, ["oob"]);
        let mut events = q.events();
        let st = q.execute("query-status", None).await.unwrap();
        assert_eq!(st["status"], "paused");
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.event, "STOP");
        assert_eq!(ev.timestamp_us, 2_000_005);
    }

    #[tokio::test]
    async fn concurrent_commands_correlate() {
        let q = client().await;
        let calls = (0..20).map(|i| {
            let q = q.clone();
            async move { q.execute("echo", Some(json!({"n": i}))).await.unwrap()["n"].as_i64() }
        });
        let results = futures_join(calls).await;
        assert_eq!(results, (0..20).map(Some).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn command_errors_are_typed() {
        let q = client().await;
        let err = q.execute("query-nothing", None).await.unwrap_err();
        assert!(err.is_command_not_found(), "{err}");
    }

    async fn futures_join<
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    >(
        fs: impl Iterator<Item = F>,
    ) -> Vec<T> {
        let handles: Vec<_> = fs.map(tokio::spawn).collect();
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }
}
