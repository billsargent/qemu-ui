//! RFB (VNC) client→server stream parser (M5-06).
//!
//! The console proxy needs to tell a viewer's *input* (keys, pointer,
//! clipboard, resize requests) from the messages that only keep its view
//! current (pixel format, encodings, update requests), so that viewers who
//! do not hold control can watch without typing. This splits the byte
//! stream a client sends into whole messages; it never alters them.
//!
//! Covered: the RFB 3.7/3.8 handshake with security types None (1) and VNC
//! authentication (2) — what QEMU offers — and every client message noVNC
//! sends, including the QEMU and extended-clipboard variants. Anything else
//! is an error: the proxy then cannot filter and treats the stream as
//! opaque (see `consoles.rs`).

/// What a complete client message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMsg {
    /// Version, security choice, auth response, ClientInit.
    Handshake,
    SetPixelFormat,
    SetEncodings,
    FramebufferUpdateRequest,
    KeyEvent,
    PointerEvent,
    ClientCutText,
    EnableContinuousUpdates,
    ClientFence,
    Xvp,
    SetDesktopSize,
    QemuExtendedKey,
    QemuAudio,
}

impl ClientMsg {
    /// Does this message act on the guest (rather than only on the view)?
    /// Viewers without control have these dropped.
    pub fn is_input(self) -> bool {
        matches!(
            self,
            ClientMsg::KeyEvent
                | ClientMsg::PointerEvent
                | ClientMsg::ClientCutText
                | ClientMsg::Xvp
                | ClientMsg::SetDesktopSize
                | ClientMsg::QemuExtendedKey
                | ClientMsg::QemuAudio
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RfbError {
    BadVersion,
    UnsupportedSecurity(u8),
    UnknownMessage(u8),
    TooLarge,
}

impl std::fmt::Display for RfbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RfbError::BadVersion => write!(f, "not an RFB 3.7/3.8 client"),
            RfbError::UnsupportedSecurity(t) => write!(f, "unsupported security type {t}"),
            RfbError::UnknownMessage(t) => write!(f, "unknown client message type {t}"),
            RfbError::TooLarge => write!(f, "client message too large"),
        }
    }
}

impl std::error::Error for RfbError {}

/// Largest message accepted (clipboard text is the only big one).
const MAX_MESSAGE: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Version,
    Security,
    VncAuth,
    ClientInit,
    Messages,
}

/// Incremental splitter for the client→server half of an RFB connection.
#[derive(Debug)]
pub struct ClientStream {
    buf: Vec<u8>,
    stage: Stage,
    error: Option<RfbError>,
}

impl Default for ClientStream {
    fn default() -> Self {
        ClientStream {
            buf: Vec::new(),
            stage: Stage::Version,
            error: None,
        }
    }
}

impl ClientStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every message completed by them, in order.
    /// Bytes of an incomplete message stay buffered until the next call.
    /// After an error (see [`ClientStream::error`]) nothing more is parsed:
    /// the stream can no longer be split, and [`ClientStream::take_rest`]
    /// hands back the unparsed bytes.
    pub fn push(&mut self, data: &[u8]) -> Vec<(ClientMsg, Vec<u8>)> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while self.error.is_none() {
            match self.next_len() {
                Ok(Some((kind, len))) => {
                    let msg: Vec<u8> = self.buf.drain(..len).collect();
                    out.push((kind, msg));
                }
                Ok(None) => break,
                Err(e) => self.error = Some(e),
            }
        }
        out
    }

    /// Why the stream stopped being parseable, if it did.
    pub fn error(&self) -> Option<&RfbError> {
        self.error.as_ref()
    }

    /// The buffered, unparsed bytes.
    pub fn take_rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    /// The kind and length of the next complete message, if buffered.
    fn next_len(&mut self) -> Result<Option<(ClientMsg, usize)>, RfbError> {
        let b = &self.buf;
        let need = |n: usize| (b.len() >= n).then_some(n);
        match self.stage {
            Stage::Version => {
                let Some(n) = need(12) else { return Ok(None) };
                match &b[..12] {
                    b"RFB 003.007\n" | b"RFB 003.008\n" => {}
                    _ => return Err(RfbError::BadVersion),
                }
                self.stage = Stage::Security;
                Ok(Some((ClientMsg::Handshake, n)))
            }
            Stage::Security => {
                let Some(n) = need(1) else { return Ok(None) };
                self.stage = match b[0] {
                    1 => Stage::ClientInit,
                    2 => Stage::VncAuth,
                    t => return Err(RfbError::UnsupportedSecurity(t)),
                };
                Ok(Some((ClientMsg::Handshake, n)))
            }
            Stage::VncAuth => {
                let Some(n) = need(16) else { return Ok(None) };
                self.stage = Stage::ClientInit;
                Ok(Some((ClientMsg::Handshake, n)))
            }
            Stage::ClientInit => {
                let Some(n) = need(1) else { return Ok(None) };
                self.stage = Stage::Messages;
                Ok(Some((ClientMsg::Handshake, n)))
            }
            Stage::Messages => {
                let Some(&t) = b.first() else { return Ok(None) };
                let (kind, len) = match t {
                    0 => (ClientMsg::SetPixelFormat, Some(20)),
                    2 => (
                        ClientMsg::SetEncodings,
                        u16_at(b, 2).map(|n| 4 + 4 * n as usize),
                    ),
                    3 => (ClientMsg::FramebufferUpdateRequest, Some(10)),
                    4 => (ClientMsg::KeyEvent, Some(8)),
                    5 => (ClientMsg::PointerEvent, Some(6)),
                    // Negative lengths are the extended-clipboard form;
                    // either way the payload is |length| bytes.
                    6 => (
                        ClientMsg::ClientCutText,
                        u32_at(b, 4).map(|n| 8 + (n as i32).unsigned_abs() as usize),
                    ),
                    150 => (ClientMsg::EnableContinuousUpdates, Some(10)),
                    248 => (ClientMsg::ClientFence, b.get(8).map(|&n| 9 + n as usize)),
                    250 => (ClientMsg::Xvp, Some(4)),
                    251 => (
                        ClientMsg::SetDesktopSize,
                        b.get(6).map(|&n| 8 + 16 * n as usize),
                    ),
                    255 => match b.get(1) {
                        None => return Ok(None),
                        Some(0) => (ClientMsg::QemuExtendedKey, Some(12)),
                        Some(1) => (
                            ClientMsg::QemuAudio,
                            u16_at(b, 2).map(|op| if op == 2 { 10 } else { 4 }),
                        ),
                        Some(_) => return Err(RfbError::UnknownMessage(255)),
                    },
                    t => return Err(RfbError::UnknownMessage(t)),
                };
                let Some(len) = len else { return Ok(None) };
                if len > MAX_MESSAGE {
                    return Err(RfbError::TooLarge);
                }
                Ok(need(len).map(|n| (kind, n)))
            }
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake(auth: bool) -> Vec<u8> {
        let mut v = b"RFB 003.008\n".to_vec();
        if auth {
            v.push(2);
            v.extend([7u8; 16]);
        } else {
            v.push(1);
        }
        v.push(1); // shared
        v
    }

    fn kinds(r: Vec<(ClientMsg, Vec<u8>)>) -> Vec<ClientMsg> {
        r.into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn handshake_then_messages_in_one_buffer() {
        let mut s = ClientStream::new();
        let mut bytes = handshake(true);
        bytes.extend([0u8; 20]); // SetPixelFormat
        bytes.extend([2, 0, 0, 2, 0, 0, 0, 7, 0xff, 0xff, 0xff, 0x21]); // 2 encodings
        bytes.extend([3, 1, 0, 0, 0, 0, 4, 0, 3, 0]); // update request
        bytes.extend([4, 1, 0, 0, 0, 0, 0, 0x61]); // key 'a' down
        bytes.extend([5, 1, 0, 10, 0, 20]); // pointer
        let got = kinds(s.push(&bytes));
        assert_eq!(
            got,
            [
                ClientMsg::Handshake,
                ClientMsg::Handshake,
                ClientMsg::Handshake,
                ClientMsg::Handshake,
                ClientMsg::SetPixelFormat,
                ClientMsg::SetEncodings,
                ClientMsg::FramebufferUpdateRequest,
                ClientMsg::KeyEvent,
                ClientMsg::PointerEvent,
            ]
        );
    }

    #[test]
    fn split_across_pushes_is_reassembled_byte_by_byte() {
        let mut bytes = handshake(false);
        bytes.extend([6, 0, 0, 0, 0, 0, 0, 5]);
        bytes.extend(b"hello");
        bytes.extend([255, 0, 0, 1, 0, 0, 0, 0x61, 0, 0, 0, 0x1e]); // QEMU key
        let mut s = ClientStream::new();
        let mut all = Vec::new();
        for b in &bytes {
            for (k, m) in s.push(&[*b]) {
                all.push((k, m));
            }
        }
        let total: usize = all.iter().map(|(_, m)| m.len()).sum();
        assert_eq!(total, bytes.len(), "nothing lost or duplicated");
        assert_eq!(
            all.iter().map(|(k, _)| *k).collect::<Vec<_>>()[3..],
            [ClientMsg::ClientCutText, ClientMsg::QemuExtendedKey]
        );
        assert_eq!(
            all[3].1,
            [6, 0, 0, 0, 0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']
        );
    }

    #[test]
    fn extended_clipboard_uses_absolute_length() {
        let mut s = ClientStream::new();
        s.push(&handshake(false));
        let len = (-3i32) as u32;
        let mut m = vec![6, 0, 0, 0];
        m.extend(len.to_be_bytes());
        m.extend([1, 2, 3]);
        m.push(3);
        m.extend([0; 9]); // and an update request after it
        assert_eq!(
            kinds(s.push(&m)),
            [
                ClientMsg::ClientCutText,
                ClientMsg::FramebufferUpdateRequest
            ]
        );
    }

    #[test]
    fn variable_length_messages() {
        let mut s = ClientStream::new();
        s.push(&handshake(false));
        let mut m = vec![248, 0, 0, 0, 0, 0, 0, 0, 2, 9, 9]; // fence, 2-byte payload
        m.extend([251, 0, 4, 0, 3, 0, 1, 0]); // SetDesktopSize, one screen
        m.extend([0u8; 16]);
        m.extend([255, 1, 0, 2, 0, 0, 0, 0, 0, 0]); // audio set-format
        m.extend([255, 1, 0, 0]); // audio enable
        m.extend([150, 1, 0, 0, 0, 0, 4, 0, 3, 0]);
        m.extend([250, 0, 1, 2]);
        let got = kinds(s.push(&m));
        assert_eq!(
            got,
            [
                ClientMsg::ClientFence,
                ClientMsg::SetDesktopSize,
                ClientMsg::QemuAudio,
                ClientMsg::QemuAudio,
                ClientMsg::EnableContinuousUpdates,
                ClientMsg::Xvp,
            ]
        );
    }

    #[test]
    fn input_classification() {
        for k in [
            ClientMsg::KeyEvent,
            ClientMsg::PointerEvent,
            ClientMsg::ClientCutText,
            ClientMsg::QemuExtendedKey,
            ClientMsg::SetDesktopSize,
        ] {
            assert!(k.is_input(), "{k:?}");
        }
        for k in [
            ClientMsg::Handshake,
            ClientMsg::SetPixelFormat,
            ClientMsg::SetEncodings,
            ClientMsg::FramebufferUpdateRequest,
            ClientMsg::EnableContinuousUpdates,
            ClientMsg::ClientFence,
        ] {
            assert!(!k.is_input(), "{k:?}");
        }
    }

    fn err_of(s: &mut ClientStream, b: &[u8]) -> RfbError {
        s.push(b);
        s.error().cloned().expect("an error")
    }

    #[test]
    fn errors_keep_completed_messages_and_the_rest() {
        assert_eq!(
            err_of(
                &mut ClientStream::new(),
                b"RFB 003.003
"
            ),
            RfbError::BadVersion
        );
        assert_eq!(
            err_of(
                &mut ClientStream::new(),
                b"RFB 003.008
"
            ),
            RfbError::UnsupportedSecurity(19)
        );
        let mut s = ClientStream::new();
        s.push(&handshake(false));
        // A complete key event, then an unknown type: the key event is kept.
        let got = s.push(&[4, 1, 0, 0, 0, 0, 0, 0x61, 42, 1, 2]);
        assert_eq!(kinds(got), [ClientMsg::KeyEvent]);
        assert_eq!(s.error(), Some(&RfbError::UnknownMessage(42)));
        assert_eq!(s.take_rest(), [42, 1, 2]);
        assert!(s.push(&[3; 10]).is_empty(), "nothing parsed after an error");
        let mut s = ClientStream::new();
        s.push(&handshake(false));
        assert_eq!(
            err_of(&mut s, &[6, 0, 0, 0, 0x7f, 0, 0, 0]),
            RfbError::TooLarge
        );
    }
}
