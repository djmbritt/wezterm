//! The Rex terminal data stream (`rex.data.terminal.v1`).
//!
//! A data socket starts with a binary handshake. The server then sends a
//! libghostty snapshot (`GHOSTSNP` envelope, ending with a FINISH record)
//! followed by the live VT output. Rex interleaves transport metadata
//! (resize, theme, child exit) into the output as an APC carrying the
//! channel's nonce: `ESC _ rex1;<nonce>;<json> ESC \`.
//!
//! Ported from rex-linux (`src/wire.rs`, `src/terminal.rs`,
//! <https://github.com/shuv1337/rex-rune>, MIT). Snapshot *decoding* is left
//! to the pane's libghostty terminal; here we only find where the snapshot
//! ends.
use crate::transport::Socket;
use anyhow::{bail, ensure, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

pub const PROTOCOL: &str = "rex.data.terminal.v1";

/// The server refused a ticket or resume token.
#[derive(Debug)]
pub struct HandshakeRejected(pub u8);
impl std::fmt::Display for HandshakeRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rex rejected data handshake (reason {})", self.0)
    }
}
impl std::error::Error for HandshakeRejected {}

#[derive(Clone, Debug)]
pub struct Welcome {
    pub token: [u8; 16],
    pub nonce: u64,
    pub retention_ms: u32,
}

pub async fn handshake(socket: &mut Socket, ticket: &str) -> Result<Welcome> {
    let ticket = URL_SAFE_NO_PAD.decode(ticket)?;
    ensure!(ticket.len() == 16, "invalid data ticket length");
    let mut message = vec![1];
    message.extend(ticket);
    exchange(socket, message).await
}

pub async fn resume(socket: &mut Socket, token: &[u8; 16], offset: u64) -> Result<Welcome> {
    let mut message = vec![2];
    message.extend(token);
    message.extend(offset.to_le_bytes());
    exchange(socket, message).await
}

async fn exchange(socket: &mut Socket, message: Vec<u8>) -> Result<Welcome> {
    socket.send(Message::Binary(message.into())).await?;
    let bytes = tokio::time::timeout(Duration::from_secs(8), next_binary(socket)).await??;
    parse_welcome(&bytes)
}

pub fn parse_welcome(bytes: &[u8]) -> Result<Welcome> {
    if bytes.first() == Some(&0x82) {
        return Err(HandshakeRejected(*bytes.get(1).unwrap_or(&0)).into());
    }
    ensure!(
        bytes.len() == 32 && bytes[0] == 0x81,
        "invalid data WELCOME"
    );
    let nonce = u64::from_le_bytes(bytes[24..32].try_into()?);
    ensure!(nonce != 0, "invalid channel nonce");
    Ok(Welcome {
        token: bytes[1..17].try_into()?,
        nonce,
        retention_ms: u32::from_le_bytes(bytes[20..24].try_into()?),
    })
}

/// The data socket closed. Code 4000 means the terminal is gone.
#[derive(Debug)]
pub struct DataClosed {
    pub code: Option<u16>,
    pub reason: String,
}
impl std::fmt::Display for DataClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "data connection closed ({:?}): {}",
            self.code, self.reason
        )
    }
}
impl std::error::Error for DataClosed {}

pub async fn next_binary(socket: &mut Socket) -> Result<Vec<u8>> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(bytes.to_vec()),
            Some(Ok(Message::Close(frame))) => {
                return Err(DataClosed {
                    code: frame.as_ref().map(|f| f.code.into()),
                    reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                }
                .into());
            }
            Some(Ok(_)) => socket.flush().await?,
            Some(Err(error)) => return Err(error.into()),
            None => bail!("data connection ended"),
        }
    }
}

const SNAPSHOT_LIMIT: usize = 256 * 1024 * 1024;
const ENVELOPE: &[u8] = b"GHOSTSNP\x01\x00";
const RECORD_HEADER: usize = 10;
const FINISH_TAG: u16 = 6;

/// Collects the snapshot at the start of a stream and finds where it ends,
/// without relying on WebSocket frame boundaries. Each record has a 10 byte
/// header: a little-endian `u16` tag, a `u32` payload length and a CRC. The
/// FINISH record (tag 6) ends the snapshot. libghostty validates the order,
/// payloads and CRCs when the pane decodes it.
#[derive(Default)]
pub struct SnapshotBuffer {
    bytes: Vec<u8>,
    scan: usize,
}

impl SnapshotBuffer {
    /// Add bytes. Once the snapshot is complete, returns it together with
    /// whatever followed it in `bytes` (live output).
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        ensure!(
            self.bytes.len() + bytes.len() <= SNAPSHOT_LIMIT,
            "snapshot exceeds 256 MiB"
        );
        self.bytes.extend_from_slice(bytes);
        if self.scan == 0 {
            if self.bytes.len() < ENVELOPE.len() {
                return Ok(None);
            }
            ensure!(
                &self.bytes[..ENVELOPE.len()] == ENVELOPE,
                "unsupported Ghostty snapshot envelope"
            );
            self.scan = ENVELOPE.len();
        }
        while self.bytes.len() >= self.scan + RECORD_HEADER {
            let header = &self.bytes[self.scan..self.scan + RECORD_HEADER];
            let tag = u16::from_le_bytes([header[0], header[1]]);
            let len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
            ensure!(
                len <= SNAPSHOT_LIMIT - self.scan - RECORD_HEADER,
                "snapshot record exceeds limit"
            );
            let end = self.scan + RECORD_HEADER + len;
            if end > self.bytes.len() {
                return Ok(None);
            }
            self.scan = end;
            if tag == FINISH_TAG {
                let tail = self.bytes.split_off(end);
                let snapshot = std::mem::take(&mut self.bytes);
                self.scan = 0;
                return Ok(Some((snapshot, tail)));
            }
        }
        Ok(None)
    }
}

/// Live output split into terminal bytes and Rex metadata.
#[derive(Debug, PartialEq)]
pub enum Part {
    Output(Vec<u8>),
    Metadata(Value),
}

/// Separates Rex metadata APCs from terminal output. Partial escape
/// prefixes are retained across frames and reconnects. Only an APC with this
/// channel's nonce is metadata; anything else is terminal output.
pub struct ApcParser {
    prefix: Vec<u8>,
    pending: Vec<u8>,
}

impl ApcParser {
    pub fn new(nonce: u64) -> Self {
        Self {
            prefix: format!("\x1b_rex1;{nonce};").into_bytes(),
            pending: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Part>> {
        let mut parts = Vec::new();
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.pending.is_empty() && byte != 0x1b {
                // Fast path: not part of a possible metadata prefix.
                output.push(byte);
                continue;
            }
            self.pending.push(byte);
            loop {
                if self.pending.starts_with(&self.prefix) {
                    ensure!(self.pending.len() <= 1024 * 1024, "Rex APC exceeds 1 MiB");
                    if self.pending.ends_with(b"\x1b\\") {
                        if !output.is_empty() {
                            parts.push(Part::Output(std::mem::take(&mut output)));
                        }
                        let json = &self.pending[self.prefix.len()..self.pending.len() - 2];
                        parts.push(Part::Metadata(serde_json::from_slice(json)?));
                        self.pending.clear();
                    }
                    break;
                }
                if self.prefix.starts_with(&self.pending) {
                    break;
                }
                output.push(self.pending.remove(0));
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        if !output.is_empty() {
            parts.push(Part::Output(output));
        }
        Ok(parts)
    }
}

/// An RGB colour from a theme, parsed from `#rrggbb`.
pub type Rgb = (u8, u8, u8);

pub fn parse_color(value: Option<&str>) -> Option<Rgb> {
    let hex = value?.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

#[cfg(test)]
mod test {
    use super::*;

    fn record(tag: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_le_bytes().to_vec();
        out.extend((payload.len() as u32).to_le_bytes());
        out.extend([0u8; 4]);
        out.extend(payload);
        out
    }

    #[test]
    fn snapshot_ends_at_finish_across_frames() {
        let mut snap = ENVELOPE.to_vec();
        snap.extend(record(1, b"meta"));
        snap.extend(record(3, &[7u8; 100]));
        snap.extend(record(FINISH_TAG, b""));
        let mut stream = snap.clone();
        stream.extend(b"hello");

        let mut buf = SnapshotBuffer::default();
        let mut result = None;
        for chunk in stream.chunks(7) {
            if let Some(done) = buf.push(chunk).unwrap() {
                result = Some(done);
                break;
            }
        }
        let (got, tail) = result.expect("snapshot completes");
        assert_eq!(got, snap);
        // The chunk that completed the snapshot may also carry output.
        assert!(b"hello".starts_with(&tail));
    }

    #[test]
    fn snapshot_rejects_unknown_envelope() {
        let mut buf = SnapshotBuffer::default();
        assert!(buf.push(b"NOTASNAP\x01\x00").is_err());
    }

    #[test]
    fn apc_metadata_is_split_out_with_partial_prefixes() {
        let mut p = ApcParser::new(42);
        let parts = p.feed(b"ab\x1b_rex1;4").unwrap();
        assert_eq!(parts, vec![Part::Output(b"ab".to_vec())]);
        let parts = p
            .feed(b"2;{\"type\":\"resize\",\"cols\":80,\"rows\":24}\x1b\\cd")
            .unwrap();
        assert_eq!(
            parts,
            vec![
                Part::Metadata(serde_json::json!({"type":"resize","cols":80,"rows":24})),
                Part::Output(b"cd".to_vec()),
            ]
        );
    }

    #[test]
    fn other_escapes_and_nonces_are_output() {
        let mut p = ApcParser::new(42);
        let parts = p.feed(b"\x1b[31mred\x1b_rex1;7;{}\x1b\\").unwrap();
        let out: Vec<u8> = parts
            .into_iter()
            .flat_map(|p| match p {
                Part::Output(b) => b,
                Part::Metadata(_) => panic!("wrong nonce must not be metadata"),
            })
            .collect();
        assert_eq!(out, b"\x1b[31mred\x1b_rex1;7;{}\x1b\\".to_vec());
    }

    #[test]
    fn welcome_parsing() {
        let mut w = vec![0x81];
        w.extend([9u8; 16]);
        w.extend([0u8; 3]);
        w.extend(5000u32.to_le_bytes());
        w.extend(77u64.to_le_bytes());
        // 1 + 16 + 3 + 4 + 8 = 32
        let welcome = parse_welcome(&w).unwrap();
        assert_eq!(welcome.nonce, 77);
        assert_eq!(welcome.retention_ms, 5000);
        assert!(parse_welcome(&[0x82, 3])
            .unwrap_err()
            .is::<HandshakeRejected>());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color(Some("#1a2B3c")), Some((0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_color(Some("1a2b3c")), None);
        assert_eq!(parse_color(None), None);
    }
}
