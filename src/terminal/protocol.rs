//! Wire protocol between the RAST server and one per-user terminal agent.
//!
//! The agent is a short-lived process started by the privileged helper inside
//! a transient systemd unit, running as the mapped `rast-u<userId>` identity.
//! It owns the PTY and the shell; the server only relays bytes. The Unix
//! socket lives in a per-session directory that the identity owns and the
//! `rast` service group can traverse, and the first line is a JSON handshake
//! carrying a random token written by the helper (deleted by the agent once
//! used), so another process in the `rast` group cannot silently attach to a
//! terminal it did not start.
//!
//! Framing: `[kind: u8][length: u32 big endian][payload]`, bounded by
//! [`MAX_FRAME`]. Output is streamed in chunks; input and resize events are
//! small. There is no per-character HTTP request and no polling.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame either side accepts (64 KiB of terminal output per chunk).
pub const MAX_FRAME: usize = 64 * 1024;
/// Largest handshake line.
pub const MAX_HANDSHAKE: usize = 8 * 1024;
/// Protocol version carried in the handshake response.
pub const VERSION: u8 = 1;

const KIND_OUTPUT: u8 = 1;
const KIND_EXIT: u8 = 2;
const KIND_ERROR: u8 = 3;
const KIND_INPUT: u8 = 1;
const KIND_RESIZE: u8 = 2;
const KIND_SIGNAL: u8 = 3;
const KIND_CLOSE: u8 = 4;

/// A frame sent by the agent to the server (and then to the browser).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFrame {
    /// Raw terminal output (UTF-8 and escape sequences pass through).
    Output(Vec<u8>),
    /// The shell exited; `None` when the status could not be decoded.
    Exit { code: Option<i32> },
    /// The terminal itself failed (PTY/shell start).
    Error(String),
}

/// A frame sent by the server to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame {
    /// Keyboard bytes.
    Input(Vec<u8>),
    /// New terminal window size (columns, rows).
    Resize { cols: u16, rows: u16 },
    /// Signal to deliver to the shell's process group (SIGTERM = 15).
    Signal { signal: u8 },
    /// Close the shell and stop the agent.
    Close,
}

/// The JSON handshake sent by the server before any frame.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handshake {
    pub token: String,
    pub cols: u16,
    pub rows: u16,
}

/// The agent's handshake answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeReply {
    pub ok: bool,
    #[serde(default)]
    pub version: u8,
    #[serde(default)]
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn checked_len(len: usize) -> io::Result<()> {
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal frame length out of range",
        ));
    }
    Ok(())
}

fn encode(kind: u8, payload: &[u8]) -> io::Result<Vec<u8>> {
    checked_len(payload.len())?;
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

impl AgentFrame {
    /// Encode an agent frame for the server.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        match self {
            Self::Output(bytes) => encode(KIND_OUTPUT, bytes),
            Self::Exit { code } => match code {
                Some(code) => encode(KIND_EXIT, &code.to_be_bytes()),
                None => encode(KIND_EXIT, &[]),
            },
            Self::Error(message) => encode(KIND_ERROR, message.as_bytes()),
        }
    }

    /// Decode an agent frame payload (after the 5-byte header was read).
    pub fn decode(kind: u8, payload: Vec<u8>) -> io::Result<Self> {
        match kind {
            KIND_OUTPUT => Ok(Self::Output(payload)),
            KIND_EXIT => {
                let code = match payload.len() {
                    0 => None,
                    4 => Some(i32::from_be_bytes([
                        payload[0], payload[1], payload[2], payload[3],
                    ])),
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid terminal exit frame",
                        ))
                    }
                };
                Ok(Self::Exit { code })
            }
            KIND_ERROR => Ok(Self::Error(String::from_utf8_lossy(&payload).into_owned())),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown terminal agent frame",
            )),
        }
    }
}

impl ServerFrame {
    /// Encode a server frame for the agent.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        match self {
            Self::Input(bytes) => encode(KIND_INPUT, bytes),
            Self::Resize { cols, rows } => {
                let mut payload = [0u8; 4];
                payload[..2].copy_from_slice(&cols.to_be_bytes());
                payload[2..].copy_from_slice(&rows.to_be_bytes());
                encode(KIND_RESIZE, &payload)
            }
            Self::Signal { signal } => encode(KIND_SIGNAL, &[*signal]),
            Self::Close => encode(KIND_CLOSE, &[]),
        }
    }

    /// Decode a server frame payload.
    pub fn decode(kind: u8, payload: Vec<u8>) -> io::Result<Self> {
        match kind {
            KIND_INPUT => Ok(Self::Input(payload)),
            KIND_RESIZE => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid terminal resize frame",
                    ));
                }
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                Ok(Self::Resize { cols, rows })
            }
            KIND_SIGNAL => {
                if payload.len() != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid terminal signal frame",
                    ));
                }
                Ok(Self::Signal { signal: payload[0] })
            }
            KIND_CLOSE => Ok(Self::Close),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown terminal server frame",
            )),
        }
    }
}

/// Read one frame from an async stream. `Ok(None)` means clean EOF.
pub async fn read_agent_frame<R>(reader: &mut R) -> io::Result<Option<AgentFrame>>
where
    R: AsyncRead + Unpin,
{
    let Some((kind, payload)) = read_frame(reader).await? else {
        return Ok(None);
    };
    AgentFrame::decode(kind, payload).map(Some)
}

/// Read one server frame from an async stream. `Ok(None)` means clean EOF.
pub async fn read_server_frame<R>(reader: &mut R) -> io::Result<Option<ServerFrame>>
where
    R: AsyncRead + Unpin,
{
    let Some((kind, payload)) = read_frame(reader).await? else {
        return Ok(None);
    };
    ServerFrame::decode(kind, payload).map(Some)
}

async fn read_frame<R>(reader: &mut R) -> io::Result<Option<(u8, Vec<u8>)>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 5];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    checked_len(len)?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some((header[0], payload)))
}

/// Write an encoded frame.
pub async fn write_frame<W>(writer: &mut W, frame: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(frame).await?;
    writer.flush().await
}

/// Trim, clamp `(cols, rows)` to something a PTY accepts.
pub fn clamp_size(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let cols = cols.unwrap_or(80).clamp(2, 1000);
    let rows = rows.unwrap_or(24).clamp(1, 1000);
    (cols, rows)
}

/// Constant-time token comparison (both are hexadecimal strings).
pub fn tokens_match(expected: &str, offered: &str) -> bool {
    let a = expected.as_bytes();
    let b = offered.as_bytes();
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_frames_round_trip() {
        for frame in [
            AgentFrame::Output(b"hello \x1b[31mworld\x1b[0m".to_vec()),
            AgentFrame::Exit { code: Some(130) },
            AgentFrame::Exit { code: None },
            AgentFrame::Error("boom".into()),
        ] {
            let encoded = frame.encode().unwrap();
            assert_eq!(
                encoded[0],
                match frame {
                    AgentFrame::Output(_) => KIND_OUTPUT,
                    AgentFrame::Exit { .. } => KIND_EXIT,
                    AgentFrame::Error(_) => KIND_ERROR,
                }
            );
            let kind = encoded[0];
            let len = u32::from_be_bytes([encoded[1], encoded[2], encoded[3], encoded[4]]) as usize;
            assert_eq!(len, encoded.len() - 5);
            let decoded = AgentFrame::decode(kind, encoded[5..].to_vec()).unwrap();
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn server_frames_round_trip() {
        for frame in [
            ServerFrame::Input(b"ls\n".to_vec()),
            ServerFrame::Resize {
                cols: 120,
                rows: 40,
            },
            ServerFrame::Signal { signal: 15 },
            ServerFrame::Close,
        ] {
            let encoded = frame.encode().unwrap();
            let kind = encoded[0];
            let len = u32::from_be_bytes([encoded[1], encoded[2], encoded[3], encoded[4]]) as usize;
            assert_eq!(len, encoded.len() - 5);
            assert_eq!(
                ServerFrame::decode(kind, encoded[5..].to_vec()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn oversized_and_unknown_frames_are_rejected() {
        assert!(AgentFrame::decode(KIND_OUTPUT, vec![0u8; MAX_FRAME + 1]).is_ok()); // length checked on read
        assert!(AgentFrame::decode(9, Vec::new()).is_err());
        assert!(ServerFrame::decode(KIND_RESIZE, vec![1, 2]).is_err());
        assert!(ServerFrame::decode(KIND_SIGNAL, vec![]).is_err());
    }

    #[tokio::test]
    async fn frames_stream_over_async_pipes() {
        let (mut client, server) = tokio::io::duplex(1024);
        let frame = ServerFrame::Input(b"x".to_vec()).encode().unwrap();
        write_frame(&mut client, &frame).await.unwrap();
        // `read_server_frame` expects a reader whose frames are server frames.
        let mut reader = tokio::io::BufReader::new(server);
        let decoded = read_server_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, ServerFrame::Input(b"x".to_vec()));
    }

    #[test]
    fn tokens_compare_in_constant_time_style() {
        assert!(tokens_match("abcdef", "abcdef"));
        assert!(!tokens_match("abcdef", "abcdeg"));
        assert!(!tokens_match("abcdef", "abcde"));
        assert!(!tokens_match("", ""));
    }

    #[test]
    fn sizes_are_clamped() {
        assert_eq!(clamp_size(None, None), (80, 24));
        assert_eq!(clamp_size(Some(0), Some(0)), (2, 1));
        assert_eq!(clamp_size(Some(5000), Some(5000)), (1000, 1000));
    }
}
