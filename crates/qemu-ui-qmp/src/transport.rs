//! Cross-platform unix-domain stream sockets.
//!
//! QEMU serves QMP and serial chardevs on AF_UNIX sockets on both Linux and
//! Windows (spike M0-14: named pipes serve a single instance and cannot be
//! reconnected after the first client leaves; loopback TCP has no access
//! control). Tokio supports AF_UNIX only on Unix, so on Windows a blocking
//! `uds_windows` socket is bridged to an async duplex stream with two threads
//! per connection.
//!
//! Socket paths must stay under 108 bytes (`sun_path`), on Windows too.

use std::io;
use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite};

/// Maximum length of an AF_UNIX path, including the terminating NUL.
pub const MAX_UNIX_PATH: usize = 108;

/// An async byte stream; what [`connect_unix`] returns.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

/// Check a socket path against the AF_UNIX length limit.
pub fn check_unix_path(path: &Path) -> io::Result<()> {
    let len = path.as_os_str().len();
    if len >= MAX_UNIX_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path is {len} bytes; AF_UNIX allows at most {} ({})",
                MAX_UNIX_PATH - 1,
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Connect to a unix-domain stream socket.
#[cfg(unix)]
pub async fn connect_unix(path: &Path) -> io::Result<Box<dyn Stream>> {
    check_unix_path(path)?;
    Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
}

/// Connect to a unix-domain stream socket.
#[cfg(windows)]
pub async fn connect_unix(path: &Path) -> io::Result<Box<dyn Stream>> {
    check_unix_path(path)?;
    let path = path.to_path_buf();
    let sock = tokio::task::spawn_blocking(move || uds_windows::UnixStream::connect(path))
        .await
        .map_err(io::Error::other)??;
    Ok(Box::new(bridge(sock)?))
}

/// Bridge a blocking socket to an async duplex stream.
#[cfg(windows)]
fn bridge(sock: uds_windows::UnixStream) -> io::Result<tokio::io::DuplexStream> {
    use std::io::{Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    let (mut dup_r, mut dup_w) = tokio::io::split(theirs);
    let rt = tokio::runtime::Handle::current();
    let mut sock_r = sock.try_clone()?;
    let mut sock_w = sock;

    let rt_in = rt.clone();
    std::thread::Builder::new()
        .name("uds-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match sock_r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if rt_in.block_on(dup_w.write_all(&buf[..n])).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = rt_in.block_on(dup_w.shutdown());
        })?;
    std::thread::Builder::new()
        .name("uds-write".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match rt.block_on(dup_r.read(&mut buf)) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock_w.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            // Wakes the reader thread too.
            let _ = sock_w.shutdown(std::net::Shutdown::Both);
        })?;
    Ok(ours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_long_paths() {
        let long = "x".repeat(120);
        assert!(check_unix_path(Path::new(&long)).is_err());
        assert!(check_unix_path(Path::new("/run/qemu-ui/vm1/qmp.sock")).is_ok());
    }
}
