//! Console recordings (plan.md §8.3, M5-03): the file format, RFB server
//! message framing, and a decoder for the encodings the recorder asks for.
//!
//! The daemon records with its own VNC client, so it controls the pixel
//! format and encodings: 32-bit true colour (R at bit 0, G at 8, B at 16,
//! little-endian) and ZRLE, CopyRect, Raw and DesktopSize. That keeps both
//! replay (noVNC decodes all of them) and server-side export (this decoder)
//! simple.
//!
//! File layout:
//!
//! ```text
//! "QUIREC01"                       magic
//! u32 BE n, n bytes JSON Header
//! records: u32 BE t_ms, u32 BE len, len bytes    (t_ms never decreases)
//! ```
//!
//! Record 0 (t = 0) is a synthetic RFB 3.8 preamble: version, security type
//! None, SecurityResult OK and the ServerInit. Every later record is one
//! complete server message. Feeding all records, in order, to an RFB client
//! replays the session; a truncated trailing record (crash) is ignored.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAGIC: &[u8; 8] = b"QUIREC01";

/// The pixel format the recorder sets (and the preamble announces).
pub const PIXEL_FORMAT: [u8; 16] = [
    32, // bits per pixel
    24, // depth
    0,  // little-endian
    1,  // true colour
    0, 255, // red max
    0, 255, // green max
    0, 255, // blue max
    0,   // red shift
    8,   // green shift
    16,  // blue shift
    0, 0, 0, // padding
];

pub const ENC_RAW: i32 = 0;
pub const ENC_COPYRECT: i32 = 1;
pub const ENC_ZRLE: i32 = 16;
pub const ENC_DESKTOP_SIZE: i32 = -223;

/// Encodings the recorder requests, in preference order.
pub const ENCODINGS: [i32; 4] = [ENC_ZRLE, ENC_COPYRECT, ENC_RAW, ENC_DESKTOP_SIZE];

/// Upper bound on a single server message the recorder accepts.
pub const MAX_MESSAGE: usize = 64 << 20;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum RecordingStatus {
    Recording,
    /// Stopped by the user or because the VM's display went away.
    Finished,
    /// Stopped at the size cap or the VM's recording quota.
    Capped,
    Failed,
    /// The daemon stopped while recording; what was written is playable.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct Bookmark {
    /// Offset from the start of the recording.
    #[ts(type = "number")]
    pub t_ms: u64,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct RecordingInfo {
    pub id: String,
    pub vm_id: String,
    /// Unix seconds.
    #[ts(type = "number")]
    pub started_at: u64,
    #[ts(type = "number | null")]
    pub ended_at: Option<u64>,
    #[ts(type = "number")]
    pub duration_ms: u64,
    #[ts(type = "number")]
    pub size_bytes: u64,
    pub status: RecordingStatus,
    pub error: Option<String>,
    pub width: u16,
    pub height: u16,
    pub bookmarks: Vec<Bookmark>,
    /// A WebM export exists.
    #[serde(default)]
    pub webm: bool,
}

/// The recording list for one VM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct RecordingsInfo {
    pub recordings: Vec<RecordingInfo>,
    /// `ffmpeg` is installed, so WebM export is offered.
    pub export_available: bool,
}

/// The JSON header at the start of a recording file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Header {
    pub version: u32,
    pub vm_id: String,
    /// Unix milliseconds.
    pub started_at_ms: u64,
    pub width: u16,
    pub height: u16,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub t_ms: u32,
    pub data: Vec<u8>,
}

pub fn encode_header(h: &Header) -> Vec<u8> {
    let json = serde_json::to_vec(h).unwrap_or_default();
    let mut out = Vec::with_capacity(12 + json.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(&json);
    out
}

pub fn encode_record(t_ms: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&t_ms.to_be_bytes());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// Parse a recording. A truncated final record is dropped, not an error.
pub fn parse(bytes: &[u8]) -> Result<(Header, Vec<Record>), String> {
    if bytes.len() < 12 || &bytes[..8] != MAGIC {
        return Err("not a qemu-ui recording".into());
    }
    let n = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let json = bytes
        .get(12..12 + n)
        .ok_or("the recording header is truncated")?;
    let header: Header =
        serde_json::from_slice(json).map_err(|e| format!("bad recording header: {e}"))?;
    let mut records = Vec::new();
    let mut at = 12 + n;
    let mut last = 0u32;
    while at + 8 <= bytes.len() {
        let t_ms = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        let len = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let Some(data) = bytes.get(at + 8..at + 8 + len) else {
            break; // truncated by a crash
        };
        if t_ms < last {
            return Err(format!("record times go backwards at byte {at}"));
        }
        last = t_ms;
        records.push(Record {
            t_ms,
            data: data.to_vec(),
        });
        at += 8 + len;
    }
    Ok((header, records))
}

/// The ServerInit message the recorder writes (and the preamble ends with).
pub fn server_init(width: u16, height: u16, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(24 + name.len());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&PIXEL_FORMAT);
    out.extend_from_slice(&(name.len() as u32).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

/// Record 0: what a server sends a client before the first update, with
/// security type None so a player needs no password.
pub fn replay_preamble(width: u16, height: u16, name: &str) -> Vec<u8> {
    let mut out = b"RFB 003.008\n".to_vec();
    out.extend_from_slice(&[1, 1]); // one security type: None
    out.extend_from_slice(&0u32.to_be_bytes()); // SecurityResult: OK
    out.extend_from_slice(&server_init(width, height, name));
    out
}

// ------------------------------------------------------------- framing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerMsg {
    /// A whole FramebufferUpdate; `resized` when it carried a DesktopSize.
    Update {
        resized: bool,
    },
    ColourMap,
    Bell,
    CutText,
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The length of the complete server message at the start of `buf`, or
/// `None` when more bytes are needed. `size` is the framebuffer size, kept
/// up to date across DesktopSize rectangles (Raw needs it only for bounds).
pub fn next_message(
    buf: &[u8],
    size: &mut (u16, u16),
) -> Result<Option<(usize, ServerMsg)>, String> {
    let Some(&kind) = buf.first() else {
        return Ok(None);
    };
    let need = |n: usize| -> Result<Option<(usize, ServerMsg)>, String> {
        if n > MAX_MESSAGE {
            Err(format!("server message of {n} bytes is too large"))
        } else {
            Ok(None)
        }
    };
    match kind {
        0 => {
            let Some(count) = be16(buf, 2) else {
                return Ok(None);
            };
            let mut at = 4;
            let mut resized = false;
            let mut dims = *size;
            for _ in 0..count {
                let Some(hdr) = buf.get(at..at + 12) else {
                    return need(at + 12);
                };
                let w = u16::from_be_bytes([hdr[4], hdr[5]]);
                let h = u16::from_be_bytes([hdr[6], hdr[7]]);
                let enc = i32::from_be_bytes(hdr[8..12].try_into().unwrap());
                at += 12;
                let body = match enc {
                    ENC_RAW => w as usize * h as usize * 4,
                    ENC_COPYRECT => 4,
                    ENC_ZRLE => match be32(buf, at) {
                        Some(n) => 4 + n as usize,
                        None => return need(at + 4),
                    },
                    ENC_DESKTOP_SIZE => {
                        dims = (w, h);
                        resized = true;
                        0
                    }
                    other => {
                        return Err(format!(
                            "unexpected encoding {other} in a framebuffer update"
                        ));
                    }
                };
                if at + body > buf.len() {
                    return need(at + body);
                }
                at += body;
            }
            *size = dims;
            Ok(Some((at, ServerMsg::Update { resized })))
        }
        1 => {
            let Some(n) = be16(buf, 4) else {
                return Ok(None);
            };
            let len = 6 + 6 * n as usize;
            if buf.len() < len {
                need(len)
            } else {
                Ok(Some((len, ServerMsg::ColourMap)))
            }
        }
        2 => Ok(Some((1, ServerMsg::Bell))),
        3 => {
            let Some(n) = be32(buf, 4) else {
                return Ok(None);
            };
            let len = 8 + n as usize;
            if buf.len() < len {
                need(len)
            } else {
                Ok(Some((len, ServerMsg::CutText)))
            }
        }
        other => Err(format!("unexpected server message type {other}")),
    }
}

// ------------------------------------------------------------- decoding

/// Rebuilds the framebuffer (RGBA) from recorded server messages.
pub struct Decoder {
    pub width: u16,
    pub height: u16,
    /// RGBA, `width * height * 4` bytes.
    pub fb: Vec<u8>,
    inflate: flate2::Decompress,
}

impl Decoder {
    pub fn new(width: u16, height: u16) -> Decoder {
        Decoder {
            width,
            height,
            fb: vec![0; width as usize * height as usize * 4],
            inflate: flate2::Decompress::new(true),
        }
    }

    fn resize(&mut self, w: u16, h: u16) {
        let mut fb = vec![0; w as usize * h as usize * 4];
        for y in 0..h.min(self.height) as usize {
            let n = w.min(self.width) as usize * 4;
            let src = y * self.width as usize * 4;
            fb[y * w as usize * 4..][..n].copy_from_slice(&self.fb[src..src + n]);
        }
        self.width = w;
        self.height = h;
        self.fb = fb;
    }

    fn put(&mut self, x: usize, y: usize, rgb: [u8; 3]) {
        if x < self.width as usize && y < self.height as usize {
            let i = (y * self.width as usize + x) * 4;
            self.fb[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }

    /// Apply one complete server message (others than updates are ignored).
    pub fn apply(&mut self, msg: &[u8]) -> Result<(), String> {
        if msg.first() != Some(&0) {
            return Ok(());
        }
        let count = be16(msg, 2).ok_or("short update")?;
        let mut at = 4;
        for _ in 0..count {
            let hdr = msg.get(at..at + 12).ok_or("short rectangle header")?;
            let x = u16::from_be_bytes([hdr[0], hdr[1]]) as usize;
            let y = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
            let w = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
            let h = u16::from_be_bytes([hdr[6], hdr[7]]) as usize;
            let enc = i32::from_be_bytes(hdr[8..12].try_into().unwrap());
            at += 12;
            match enc {
                ENC_RAW => {
                    let data = msg.get(at..at + w * h * 4).ok_or("short raw rectangle")?;
                    for row in 0..h {
                        for col in 0..w {
                            let p = &data[(row * w + col) * 4..][..3];
                            self.put(x + col, y + row, [p[0], p[1], p[2]]);
                        }
                    }
                    at += w * h * 4;
                }
                ENC_COPYRECT => {
                    let sx = be16(msg, at).ok_or("short copyrect")? as usize;
                    let sy = be16(msg, at + 2).ok_or("short copyrect")? as usize;
                    at += 4;
                    let (fw, fh) = (self.width as usize, self.height as usize);
                    if sx + w > fw || sy + h > fh || x + w > fw || y + h > fh {
                        return Err("copyrect outside the framebuffer".into());
                    }
                    let src: Vec<u8> = (0..h)
                        .flat_map(|r| self.fb[((sy + r) * fw + sx) * 4..][..w * 4].to_vec())
                        .collect();
                    for r in 0..h {
                        self.fb[((y + r) * fw + x) * 4..][..w * 4]
                            .copy_from_slice(&src[r * w * 4..][..w * 4]);
                    }
                }
                ENC_ZRLE => {
                    let n = be32(msg, at).ok_or("short zrle length")? as usize;
                    let input = msg.get(at + 4..at + 4 + n).ok_or("short zrle data")?;
                    at += 4 + n;
                    let data = self.inflate_all(input)?;
                    self.zrle(&data, x, y, w, h)?;
                }
                ENC_DESKTOP_SIZE => self.resize(w as u16, h as u16),
                other => return Err(format!("unsupported encoding {other}")),
            }
        }
        Ok(())
    }

    fn inflate_all(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(input.len() * 4 + 1024);
        let mut consumed = 0;
        loop {
            if out.len() == out.capacity() {
                out.reserve(out.capacity().max(64 * 1024));
            }
            let before_in = self.inflate.total_in();
            let before_out = self.inflate.total_out();
            self.inflate
                .decompress_vec(&input[consumed..], &mut out, flate2::FlushDecompress::Sync)
                .map_err(|e| format!("zrle inflate: {e}"))?;
            let used = (self.inflate.total_in() - before_in) as usize;
            consumed += used;
            let produced = self.inflate.total_out() - before_out;
            // Done once all input is in and the output wasn't cut short.
            if consumed >= input.len() && (out.len() < out.capacity() || produced == 0) {
                return Ok(out);
            }
            if used == 0 && produced == 0 {
                return Err("zrle inflate made no progress".into());
            }
            if out.len() > MAX_MESSAGE * 4 {
                return Err("zrle data inflates too far".into());
            }
        }
    }

    fn zrle(&mut self, d: &[u8], rx: usize, ry: usize, w: usize, h: usize) -> Result<(), String> {
        let mut at = 0;
        let byte = |at: &mut usize| -> Result<u8, String> {
            let b = *d.get(*at).ok_or("short zrle tile")?;
            *at += 1;
            Ok(b)
        };
        let cpixel = |at: &mut usize| -> Result<[u8; 3], String> {
            let p = d.get(*at..*at + 3).ok_or("short zrle pixel")?;
            *at += 3;
            Ok([p[0], p[1], p[2]])
        };
        for ty in (0..h).step_by(64) {
            for tx in (0..w).step_by(64) {
                let tw = 64.min(w - tx);
                let th = 64.min(h - ty);
                let sub = byte(&mut at)?;
                let (ox, oy) = (rx + tx, ry + ty);
                match sub {
                    0 => {
                        for r in 0..th {
                            for c in 0..tw {
                                let p = cpixel(&mut at)?;
                                self.put(ox + c, oy + r, p);
                            }
                        }
                    }
                    1 => {
                        let p = cpixel(&mut at)?;
                        for r in 0..th {
                            for c in 0..tw {
                                self.put(ox + c, oy + r, p);
                            }
                        }
                    }
                    2..=16 => {
                        let palette: Vec<[u8; 3]> = (0..sub)
                            .map(|_| cpixel(&mut at))
                            .collect::<Result<_, _>>()?;
                        let bits = match sub {
                            2 => 1,
                            3..=4 => 2,
                            _ => 4,
                        };
                        for r in 0..th {
                            let mut cur = 0u8;
                            let mut left = 0;
                            for c in 0..tw {
                                if left == 0 {
                                    cur = byte(&mut at)?;
                                    left = 8;
                                }
                                left -= bits;
                                let idx = ((cur >> left) & ((1 << bits) - 1)) as usize;
                                let p =
                                    *palette.get(idx).ok_or("zrle palette index out of range")?;
                                self.put(ox + c, oy + r, p);
                            }
                        }
                    }
                    128 | 130..=255 => {
                        let palette: Vec<[u8; 3]> = if sub == 128 {
                            vec![]
                        } else {
                            (0..sub - 128)
                                .map(|_| cpixel(&mut at))
                                .collect::<Result<_, _>>()?
                        };
                        let mut i = 0;
                        while i < tw * th {
                            let (p, run) = if sub == 128 {
                                (cpixel(&mut at)?, true)
                            } else {
                                let idx = byte(&mut at)?;
                                let p = *palette
                                    .get((idx & 0x7f) as usize)
                                    .ok_or("zrle palette index out of range")?;
                                (p, idx & 0x80 != 0)
                            };
                            let mut len = 1;
                            if run {
                                loop {
                                    let b = byte(&mut at)? as usize;
                                    len += b;
                                    if b != 255 {
                                        break;
                                    }
                                }
                            }
                            if i + len > tw * th {
                                return Err("zrle run overflows its tile".into());
                            }
                            for k in i..i + len {
                                self.put(ox + k % tw, oy + k / tw, p);
                            }
                            i += len;
                        }
                    }
                    other => return Err(format!("unknown zrle subencoding {other}")),
                }
            }
        }
        Ok(())
    }
}

/// Frames for export: the framebuffer at every `1/fps` step, cropped or
/// padded to the recording's initial size. Calls `frame` with RGBA bytes.
pub fn render_frames(
    header: &Header,
    records: &[Record],
    fps: u32,
    mut frame: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<u64, String> {
    let (cw, ch) = (header.width as usize, header.height as usize);
    let mut dec = Decoder::new(header.width, header.height);
    let step = Duration::from_millis(1000 / fps.max(1) as u64).as_millis() as u64;
    let end = records.last().map_or(0, |r| r.t_ms as u64);
    let mut canvas = vec![0u8; cw * ch * 4];
    let mut next = 0usize;
    let mut frames = 0u64;
    let mut t = 0u64;
    loop {
        // Record 0 is the replay preamble, not a server message.
        while next < records.len() && records[next].t_ms as u64 <= t {
            if next > 0 {
                dec.apply(&records[next].data)?;
            }
            next += 1;
        }
        let (fw, fh) = (dec.width as usize, dec.height as usize);
        for y in 0..ch {
            let row = &mut canvas[y * cw * 4..][..cw * 4];
            if y < fh {
                let n = cw.min(fw) * 4;
                row[..n].copy_from_slice(&dec.fb[y * fw * 4..][..n]);
                row[n..].fill(0);
            } else {
                row.fill(0);
            }
        }
        frame(&canvas)?;
        frames += 1;
        if t >= end {
            break;
        }
        t += step;
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn header() -> Header {
        Header {
            version: 1,
            vm_id: "vm1".into(),
            started_at_ms: 1,
            width: 4,
            height: 2,
            name: "QEMU".into(),
        }
    }

    #[test]
    fn file_round_trip_and_truncation() {
        let mut f = encode_header(&header());
        f.extend(encode_record(0, &replay_preamble(4, 2, "QEMU")));
        f.extend(encode_record(10, b"\x02"));
        let (h, recs) = parse(&f).unwrap();
        assert_eq!(h, header());
        assert_eq!(recs.len(), 2);
        assert_eq!(
            recs[1],
            Record {
                t_ms: 10,
                data: vec![2]
            }
        );
        // A crash mid-record loses only that record.
        let mut cut = f.clone();
        cut.extend(&encode_record(20, b"\x02\x02\x02")[..9]);
        assert_eq!(parse(&cut).unwrap().1.len(), 2);
        assert!(parse(b"nope").is_err());
        let mut back = f.clone();
        back.extend(encode_record(5, b"\x02"));
        assert!(parse(&back).is_err(), "time must not go backwards");
    }

    #[test]
    fn preamble_is_rfb_38_with_no_security() {
        let p = replay_preamble(800, 600, "x");
        assert!(p.starts_with(b"RFB 003.008\n"));
        assert_eq!(&p[12..14], &[1, 1]);
        assert_eq!(&p[14..18], &[0, 0, 0, 0]);
        assert_eq!(&p[18..20], &800u16.to_be_bytes());
        assert_eq!(&p[22..38], &PIXEL_FORMAT);
    }

    fn rect(x: u16, y: u16, w: u16, h: u16, enc: i32) -> Vec<u8> {
        let mut r = vec![];
        for v in [x, y, w, h] {
            r.extend_from_slice(&v.to_be_bytes());
        }
        r.extend_from_slice(&enc.to_be_bytes());
        r
    }

    fn update(rects: &[Vec<u8>]) -> Vec<u8> {
        let mut m = vec![0, 0];
        m.extend_from_slice(&(rects.len() as u16).to_be_bytes());
        for r in rects {
            m.extend_from_slice(r);
        }
        m
    }

    #[test]
    fn framing_waits_for_whole_messages() {
        let mut raw = rect(0, 0, 2, 1, ENC_RAW);
        raw.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        let msg = update(&[raw, rect(0, 0, 8, 8, ENC_DESKTOP_SIZE)]);
        let mut size = (4, 2);
        for cut in 0..msg.len() {
            assert_eq!(
                next_message(&msg[..cut], &mut size).unwrap(),
                None,
                "cut at {cut}"
            );
        }
        assert_eq!(size, (4, 2));
        let got = next_message(&msg, &mut size).unwrap();
        assert_eq!(got, Some((msg.len(), ServerMsg::Update { resized: true })));
        assert_eq!(size, (8, 8));
        assert_eq!(
            next_message(&[2], &mut size).unwrap(),
            Some((1, ServerMsg::Bell))
        );
        assert_eq!(
            next_message(&[3, 0, 0, 0, 0, 0, 0, 2, b'h', b'i'], &mut size).unwrap(),
            Some((10, ServerMsg::CutText))
        );
        assert!(next_message(&[9], &mut size).is_err());
        assert!(next_message(&update(&[rect(0, 0, 1, 1, 7)]), &mut size).is_err());
    }

    fn zlib(data: &[u8], z: &mut flate2::write::ZlibEncoder<Vec<u8>>) -> Vec<u8> {
        z.write_all(data).unwrap();
        z.flush().unwrap();
        std::mem::take(z.get_mut())
    }

    #[test]
    fn decodes_raw_copyrect_and_zrle() {
        let mut d = Decoder::new(4, 2);
        let mut raw = rect(0, 0, 1, 1, ENC_RAW);
        raw.extend_from_slice(&[10, 20, 30, 0]);
        d.apply(&update(&[raw])).unwrap();
        assert_eq!(&d.fb[..4], &[10, 20, 30, 255]);

        let mut cr = rect(3, 1, 1, 1, ENC_COPYRECT);
        cr.extend_from_slice(&[0, 0, 0, 0]);
        d.apply(&update(&[cr])).unwrap();
        assert_eq!(&d.fb[(4 + 3) * 4..][..4], &[10, 20, 30, 255]);

        // One zlib stream across two updates, as a real connection sends.
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        let solid = zlib(&[1, 7, 8, 9], &mut z); // tile: solid colour
        let mut r1 = rect(0, 0, 4, 2, ENC_ZRLE);
        r1.extend_from_slice(&(solid.len() as u32).to_be_bytes());
        r1.extend_from_slice(&solid);
        d.apply(&update(&[r1])).unwrap();
        assert!(d.fb.chunks(4).all(|p| p == [7, 8, 9, 255]));

        // Packed palette (2 colours, 1 bit) then plain RLE.
        let packed = zlib(&[2, 1, 1, 1, 2, 2, 2, 0b1010_0000, 0b0101_0000], &mut z);
        let mut r2 = rect(0, 0, 4, 2, ENC_ZRLE);
        r2.extend_from_slice(&(packed.len() as u32).to_be_bytes());
        r2.extend_from_slice(&packed);
        d.apply(&update(&[r2])).unwrap();
        let px = |d: &Decoder, i: usize| d.fb[i * 4];
        assert_eq!(
            (0..6).map(|i| px(&d, i)).collect::<Vec<_>>(),
            [2, 1, 2, 1, 1, 2]
        );

        let rle = zlib(&[128, 5, 5, 5, 6, 3, 3, 3, 0], &mut z); // 7 pixels, then 1
        let mut r3 = rect(0, 0, 4, 2, ENC_ZRLE);
        r3.extend_from_slice(&(rle.len() as u32).to_be_bytes());
        r3.extend_from_slice(&rle);
        d.apply(&update(&[r3])).unwrap();
        assert_eq!(px(&d, 6), 5);
        assert_eq!(px(&d, 7), 3);

        // Palette RLE: index 0 run of 8.
        let prle = zlib(&[130, 4, 4, 4, 9, 9, 9, 0x80, 7], &mut z);
        let mut r4 = rect(0, 0, 4, 2, ENC_ZRLE);
        r4.extend_from_slice(&(prle.len() as u32).to_be_bytes());
        r4.extend_from_slice(&prle);
        d.apply(&update(&[r4])).unwrap();
        assert!(d.fb.chunks(4).all(|p| p == [4, 4, 4, 255]));

        d.apply(&update(&[rect(0, 0, 2, 3, ENC_DESKTOP_SIZE)]))
            .unwrap();
        assert_eq!((d.width, d.height, d.fb.len()), (2, 3, 24));
    }

    #[test]
    fn frames_are_sampled_on_a_fixed_canvas() {
        let h = header();
        let mut raw = rect(0, 0, 1, 1, ENC_RAW);
        raw.extend_from_slice(&[1, 1, 1, 0]);
        let recs = vec![
            Record {
                t_ms: 0,
                data: replay_preamble(4, 2, "QEMU"),
            },
            Record {
                t_ms: 250,
                data: update(&[raw]),
            },
            Record {
                t_ms: 300,
                data: update(&[rect(0, 0, 8, 8, ENC_DESKTOP_SIZE)]),
            },
        ];
        let mut seen = vec![];
        let n = render_frames(&h, &recs, 10, |f| {
            assert_eq!(f.len(), 4 * 2 * 4);
            seen.push(f[0]);
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 4); // t = 0, 100, 200, 300
        assert_eq!(seen, [0, 0, 0, 1]);
    }
}
