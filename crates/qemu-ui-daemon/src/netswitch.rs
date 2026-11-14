//! Userspace Ethernet switch for isolated networks (plan.md §10.1, M3-04).
//!
//! Each isolated network gets one switch, run by the daemon on an AF_UNIX
//! stream socket. QEMU connects to it as a client
//! (`-netdev stream,server=off,addr.type=unix,...,reconnect-ms=...`); its
//! framing is a 4-byte big-endian length followed by the Ethernet frame.
//! Nothing touches a host interface, so guests on an isolated network see
//! each other and nothing else, on Linux and Windows alike.
//!
//! The switch learns source MACs, sends known unicast to one port and floods
//! the rest. Every port has a bounded queue; when a guest stops reading, its
//! frames are dropped rather than stalling the others (Ethernet semantics).
//!
//! Switches start with the first VM on the network and stop with the last.
//! QEMU keeps reconnecting, so after a daemon restart the reattached VMs
//! re-acquire their switches and the links come back by themselves.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// Largest frame accepted: QEMU's stream netdev sends at most 64 KiB plus
/// headers (virtio-net with offloads); anything bigger is a broken peer.
pub const MAX_FRAME: usize = 69_632;
/// Frames queued per port before further frames to it are dropped.
const PORT_QUEUE: usize = 256;
/// Shortest Ethernet header: destination, source, ethertype.
const ETH_HEADER: usize = 14;

type Frame = Arc<[u8]>;

struct Port {
    tx: mpsc::Sender<Frame>,
    tasks: Vec<AbortHandle>,
}

#[derive(Default)]
struct Fabric {
    next: u64,
    ports: HashMap<u64, Port>,
    /// Learned MAC → port.
    macs: HashMap<[u8; 6], u64>,
}

/// The switching fabric; cheap to clone, shared by all its ports.
#[derive(Clone, Default)]
pub struct Switch {
    inner: Arc<Mutex<Fabric>>,
}

impl Switch {
    /// Connect a port: frames read from `stream` are switched to the other
    /// ports, frames for this port are written to it.
    pub fn attach<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mut r, mut w) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<Frame>(PORT_QUEUE);
        let id = {
            let mut f = self.inner.lock().unwrap();
            f.next += 1;
            f.next
        };
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let len = (frame.len() as u32).to_be_bytes();
                if w.write_all(&len).await.is_err() || w.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        // Register before the reader runs, so its first frame finds the port.
        self.inner.lock().unwrap().ports.insert(
            id,
            Port {
                tx,
                tasks: vec![writer.abort_handle()],
            },
        );
        let sw = self.clone();
        let reader = tokio::spawn(async move {
            loop {
                match read_frame(&mut r).await {
                    Ok(Some(frame)) => sw.forward(id, frame.into()),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("switch port {id}: {e}");
                        break;
                    }
                }
            }
            sw.detach(id);
        });
        if let Some(p) = self.inner.lock().unwrap().ports.get_mut(&id) {
            p.tasks.push(reader.abort_handle());
        }
    }

    fn forward(&self, from: u64, frame: Frame) {
        if frame.len() < ETH_HEADER {
            return;
        }
        let dst: [u8; 6] = frame[0..6].try_into().unwrap();
        let src: [u8; 6] = frame[6..12].try_into().unwrap();
        let mut f = self.inner.lock().unwrap();
        if src[0] & 1 == 0 {
            f.macs.insert(src, from);
        }
        let known = (dst[0] & 1 == 0)
            .then(|| f.macs.get(&dst).copied())
            .flatten();
        match known {
            Some(to) if to == from => {}
            Some(to) => {
                if let Some(p) = f.ports.get(&to) {
                    let _ = p.tx.try_send(frame);
                }
            }
            None => {
                for (id, p) in &f.ports {
                    if *id != from {
                        // Full queue: the frame is lost for that port only.
                        let _ = p.tx.try_send(frame.clone());
                    }
                }
            }
        }
    }

    fn detach(&self, id: u64) {
        let mut f = self.inner.lock().unwrap();
        if let Some(p) = f.ports.remove(&id) {
            for t in p.tasks {
                t.abort();
            }
        }
        f.macs.retain(|_, p| *p != id);
    }

    /// Connected ports.
    #[cfg_attr(not(test), allow(dead_code))] // diagnostics and tests
    pub fn port_count(&self) -> usize {
        self.inner.lock().unwrap().ports.len()
    }

    /// Disconnect every port.
    pub fn shutdown(&self) {
        let mut f = self.inner.lock().unwrap();
        for (_, p) in f.ports.drain() {
            for t in p.tasks {
                t.abort();
            }
        }
        f.macs.clear();
    }
}

/// One length-prefixed frame; `None` at a clean end of stream.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {n} bytes exceeds {MAX_FRAME}"),
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

// ------------------------------------------------------------- listening

/// A switch serving its socket.
pub struct Listening {
    pub switch: Switch,
    path: PathBuf,
    accept: AbortHandle,
    #[cfg(windows)]
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Listening {
    /// Stop accepting, disconnect every port and remove the socket.
    pub fn stop(self) {
        self.accept.abort();
        #[cfg(windows)]
        {
            // The accept thread blocks in accept(); wake it so it sees the flag.
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = uds_windows::UnixStream::connect(&self.path);
        }
        self.switch.shutdown();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve a new switch on `path` (replacing a stale socket file).
#[cfg(unix)]
pub fn listen(path: &Path) -> io::Result<Listening> {
    use std::os::unix::fs::PermissionsExt;
    qemu_ui_qmp::transport::check_unix_path(path)?;
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    // QEMU runs as the daemon's user; nobody else may join the network.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let switch = Switch::default();
    let sw = switch.clone();
    let accept = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((s, _)) => sw.attach(s),
                Err(e) => {
                    tracing::warn!("switch accept: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });
    Ok(Listening {
        switch,
        path: path.to_path_buf(),
        accept: accept.abort_handle(),
    })
}

/// Serve a new switch on `path` (replacing a stale socket file).
#[cfg(windows)]
pub fn listen(path: &Path) -> io::Result<Listening> {
    use std::sync::atomic::{AtomicBool, Ordering};
    qemu_ui_qmp::transport::check_unix_path(path)?;
    let _ = std::fs::remove_file(path);
    let listener = uds_windows::UnixListener::bind(path)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = mpsc::channel::<uds_windows::UnixStream>(16);
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("netswitch-accept".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                match conn {
                    Ok(s) => {
                        if tx.blocking_send(s).is_err() {
                            break;
                        }
                    }
                    Err(e) => tracing::warn!("switch accept: {e}"),
                }
            }
        })?;
    let switch = Switch::default();
    let sw = switch.clone();
    let accept = tokio::spawn(async move {
        while let Some(s) = rx.recv().await {
            match qemu_ui_qmp::transport::bridge(s) {
                Ok(stream) => sw.attach(stream),
                Err(e) => tracing::warn!("switch port: {e}"),
            }
        }
    });
    Ok(Listening {
        switch,
        path: path.to_path_buf(),
        accept: accept.abort_handle(),
        stop,
    })
}

// ------------------------------------------------------------- per network

struct Entry {
    listening: Listening,
    users: BTreeSet<String>,
}

/// The running switches, one per isolated network, reference-counted by VM.
pub struct Switches {
    root: PathBuf,
    running: Mutex<HashMap<String, Entry>>,
}

impl Switches {
    pub fn new(root: PathBuf) -> Self {
        Switches {
            root,
            running: Mutex::default(),
        }
    }

    /// Where network `id`'s switch listens.
    pub fn socket_path(&self, network: &str) -> PathBuf {
        self.root.join(format!("net-{network}.sock"))
    }

    /// VM `vm` uses network `network`: start its switch if needed.
    pub fn acquire(&self, network: &str, vm: &str) -> io::Result<PathBuf> {
        let path = self.socket_path(network);
        let mut running = self.running.lock().unwrap();
        if !running.contains_key(network) {
            let listening = listen(&path)?;
            tracing::info!(network, "isolated network switch started");
            running.insert(
                network.to_string(),
                Entry {
                    listening,
                    users: BTreeSet::new(),
                },
            );
        }
        running
            .get_mut(network)
            .expect("inserted above")
            .users
            .insert(vm.to_string());
        Ok(path)
    }

    /// VM `vm` left every network; stop switches nobody uses any more.
    pub fn release(&self, vm: &str) {
        let mut running = self.running.lock().unwrap();
        let idle: Vec<String> = running
            .iter_mut()
            .filter_map(|(net, e)| {
                e.users.remove(vm);
                e.users.is_empty().then(|| net.clone())
            })
            .collect();
        for net in idle {
            if let Some(e) = running.remove(&net) {
                e.listening.stop();
                tracing::info!(network = %net, "isolated network switch stopped");
            }
        }
    }

    /// Connected ports of a network's switch, if it is running.
    #[cfg_attr(not(test), allow(dead_code))] // diagnostics and tests
    pub fn ports(&self, network: &str) -> Option<usize> {
        self.running
            .lock()
            .unwrap()
            .get(network)
            .map(|e| e.listening.switch.port_count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::DuplexStream;

    fn frame(dst: [u8; 6], src: [u8; 6], payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&dst);
        f.extend_from_slice(&src);
        f.extend_from_slice(&[0x88, 0xb5]); // local experimental ethertype
        f.extend_from_slice(payload);
        f
    }

    const A: [u8; 6] = [0x52, 0x54, 0, 0, 0, 0xa];
    const B: [u8; 6] = [0x52, 0x54, 0, 0, 0, 0xb];
    const C: [u8; 6] = [0x52, 0x54, 0, 0, 0, 0xc];
    const BCAST: [u8; 6] = [0xff; 6];

    async fn send(s: &mut DuplexStream, f: &[u8]) {
        s.write_all(&(f.len() as u32).to_be_bytes()).await.unwrap();
        s.write_all(f).await.unwrap();
    }

    async fn recv(s: &mut DuplexStream) -> Option<Vec<u8>> {
        tokio::time::timeout(Duration::from_millis(300), read_frame(s))
            .await
            .ok()
            .and_then(|r| r.ok().flatten())
    }

    fn port(sw: &Switch) -> DuplexStream {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        sw.attach(theirs);
        ours
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    #[tokio::test]
    async fn floods_unknown_then_learns_unicast() {
        let sw = Switch::default();
        let (mut a, mut b, mut c) = (port(&sw), port(&sw), port(&sw));
        settle().await;
        assert_eq!(sw.port_count(), 3);

        // Broadcast from A reaches B and C, not A; A is now learned.
        let hello = frame(BCAST, A, b"hello");
        send(&mut a, &hello).await;
        assert_eq!(recv(&mut b).await.as_deref(), Some(&hello[..]));
        assert_eq!(recv(&mut c).await.as_deref(), Some(&hello[..]));
        assert_eq!(recv(&mut a).await, None, "no echo");

        // B → A (known): only A gets it. B is learned too.
        let reply = frame(A, B, b"reply");
        send(&mut b, &reply).await;
        assert_eq!(recv(&mut a).await.as_deref(), Some(&reply[..]));
        assert_eq!(
            recv(&mut c).await,
            None,
            "unicast to a known MAC is not flooded"
        );

        // C → unknown D: flooded to A and B.
        let probe = frame([0x52, 0x54, 0, 0, 0, 0xd], C, b"?");
        send(&mut c, &probe).await;
        assert_eq!(recv(&mut a).await.as_deref(), Some(&probe[..]));
        assert_eq!(recv(&mut b).await.as_deref(), Some(&probe[..]));
    }

    #[tokio::test]
    async fn a_departed_port_is_forgotten() {
        let sw = Switch::default();
        let (mut a, b, mut c) = (port(&sw), port(&sw), port(&sw));
        settle().await;
        let mut b = b;
        send(&mut b, &frame(BCAST, B, b"i am b")).await;
        let _ = (recv(&mut a).await, recv(&mut c).await);
        drop(b);
        settle().await;
        assert_eq!(sw.port_count(), 2);
        // B's MAC is unknown again, so traffic to it floods.
        let f = frame(B, A, b"where are you");
        send(&mut a, &f).await;
        assert_eq!(recv(&mut c).await.as_deref(), Some(&f[..]));
    }

    #[tokio::test]
    async fn two_switches_are_isolated() {
        let (one, two) = (Switch::default(), Switch::default());
        let (mut a, mut b) = (port(&one), port(&two));
        let mut a2 = port(&one);
        settle().await;
        let f = frame(BCAST, A, b"only on one");
        send(&mut a, &f).await;
        assert_eq!(recv(&mut a2).await.as_deref(), Some(&f[..]));
        assert_eq!(recv(&mut b).await, None);
    }

    #[tokio::test]
    async fn oversized_and_runt_frames() {
        let sw = Switch::default();
        let (mut a, mut b) = (port(&sw), port(&sw));
        settle().await;
        // A runt (shorter than an Ethernet header) is ignored.
        send(&mut a, &[1, 2, 3]).await;
        assert_eq!(recv(&mut b).await, None);
        // An absurd length drops the port.
        a.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        settle().await;
        assert_eq!(sw.port_count(), 1);
    }

    #[tokio::test]
    async fn a_stalled_port_does_not_block_the_others() {
        let sw = Switch::default();
        // The stalled port's pipe is tiny and never read.
        let (_stalled, theirs) = tokio::io::duplex(64);
        sw.attach(theirs);
        let (mut a, mut b) = (port(&sw), port(&sw));
        settle().await;
        for k in 0..(PORT_QUEUE * 4) {
            send(&mut a, &frame(BCAST, A, &k.to_be_bytes())).await;
        }
        // B still receives everything in order.
        for k in 0..(PORT_QUEUE * 4) {
            let f = recv(&mut b).await.expect("frame delivered");
            assert_eq!(&f[14..], &k.to_be_bytes());
        }
    }

    #[tokio::test]
    async fn manager_starts_and_stops_with_its_users() {
        let root =
            std::env::temp_dir().join(format!("qui-nsw-{:06x}", rand::random::<u32>() & 0xff_ffff));
        std::fs::create_dir_all(&root).unwrap();
        let sws = Switches::new(root.clone());
        let p = sws.acquire("n1", "vm1").unwrap();
        assert!(p.exists());
        assert_eq!(sws.acquire("n1", "vm2").unwrap(), p);
        let conn = qemu_ui_qmp::transport::connect_unix(&p).await.unwrap();
        for _ in 0..50 {
            if sws.ports("n1") == Some(1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sws.ports("n1"), Some(1));
        sws.release("vm1");
        assert!(sws.ports("n1").is_some(), "still used by vm2");
        sws.release("vm2");
        assert_eq!(sws.ports("n1"), None);
        assert!(!p.exists(), "socket removed");
        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Two real VMs on one isolated network: both connect to its switch, a
/// guest's frames reach other ports, and the switch stops with the last VM.
#[cfg(test)]
mod real {
    use super::*;
    use crate::supervisor::test_support::*;
    use qemu_ui_core::network::NetworkConfig;
    use qemu_ui_core::spec::NicBackend;
    use qemu_ui_core::vm::VmState;
    use std::time::Duration;

    #[test]
    fn vms_on_an_isolated_network_share_a_switch() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("iso");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let lan = sv
                .networks
                .create("lan", NetworkConfig::Isolated {})
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let mut ids = vec![];
            let mut macs = vec![];
            for name in ["iso-a", "iso-b"] {
                let id = small_vm(&sv, &caps, name).await;
                let mut spec = sv.detail(&id).ok().unwrap().spec;
                spec.nics[0].backend = NicBackend::Network {
                    network: lan.id.clone(),
                    forwards: vec![],
                };
                let saved = sv
                    .update(&id, spec)
                    .await
                    .map_err(|e| format!("{e:?}"))
                    .unwrap();
                macs.push(saved.spec.nics[0].mac.clone().unwrap());
                ids.push(id);
            }
            assert_eq!(
                sv.switches.ports(&lan.id),
                None,
                "no switch before a VM starts"
            );

            for id in &ids {
                let s = sv.start(id).await.map_err(|e| format!("{e:?}")).unwrap();
                assert_eq!(s.state, VmState::Running);
            }
            // An observer port of our own on the same switch.
            let sock = sv.switches.socket_path(&lan.id);
            let mut observer = qemu_ui_qmp::transport::connect_unix(&sock).await.unwrap();
            let mut connected = 0;
            for _ in 0..100 {
                connected = sv.switches.ports(&lan.id).unwrap_or(0);
                if connected == 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert_eq!(connected, 3, "both VMs and the observer are connected");

            // A guest's broadcast (self-announcement) reaches the other ports.
            let qmp = sv.live_qmp(&ids[0]).unwrap();
            qmp.execute(
                "announce-self",
                Some(serde_json::json!({"initial": 50, "max": 550, "rounds": 2, "step": 50})),
            )
            .await
            .unwrap();
            let want: Vec<u8> = macs[0]
                .split(':')
                .map(|h| u8::from_str_radix(h, 16).unwrap())
                .collect();
            let seen = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match read_frame(&mut observer).await {
                        Ok(Some(f)) if f.len() >= 12 && f[6..12] == want[..] => return true,
                        Ok(Some(_)) => continue,
                        _ => return false,
                    }
                }
            })
            .await
            .unwrap_or(false);
            assert!(seen, "a frame from VM A's MAC crossed the switch");

            for id in &ids {
                sv.force_stop(id)
                    .await
                    .map_err(|e| format!("{e:?}"))
                    .unwrap();
                wait_state(&sv, id, VmState::Stopped).await;
            }
            assert_eq!(
                sv.switches.ports(&lan.id),
                None,
                "the switch stops with its last VM"
            );
            assert!(!sock.exists());
            drop(observer);
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
