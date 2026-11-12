//! Interactive terminals into containers (`container-exec`).
//!
//! A terminal session starts like every other worker request: one JSON line
//! in, one JSON line out. When the worker accepts the session it answers with
//! a success line and the same connection switches to binary frames until the
//! shell exits or either side closes:
//!
//! ```text
//! [kind: u8][length: u32 big endian][payload: length bytes]
//! ```
//!
//! | kind | direction        | payload                                  |
//! |------|------------------|------------------------------------------|
//! | 1    | both             | terminal bytes (keyboard in, screen out) |
//! | 2    | server → worker  | new size: cols u16 BE, rows u16 BE        |
//! | 3    | worker → server  | exit status of the shell: i32 BE         |
//! | 4    | server → worker  | close the session                         |
//!
//! This module also holds [`terminal_policy`], shared by the API (to explain
//! why a terminal is unavailable) and by the worker (which re-checks it right
//! before it starts the shell).

use std::io;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Terminal bytes, in either direction.
pub const FRAME_DATA: u8 = 1;
/// Window size change (server → worker).
pub const FRAME_RESIZE: u8 = 2;
/// The shell exited (worker → server).
pub const FRAME_EXIT: u8 = 3;
/// Close the session (server → worker).
pub const FRAME_CLOSE: u8 = 4;

/// Largest payload of one frame.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

/// Terminal size bounds; anything outside is clamped.
pub const MIN_COLS: u16 = 10;
pub const MAX_COLS: u16 = 500;
pub const MIN_ROWS: u16 = 2;
pub const MAX_ROWS: u16 = 200;
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// At most this many terminals are open at the same time on one server.
pub const MAX_EXEC_SESSIONS: usize = 4;

/// A terminal without keyboard input for this long is closed.
pub const EXEC_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The command started inside the container: bash when the image has it,
/// otherwise the POSIX shell. Fixed text; nothing from the client is added.
pub const SHELL_SCRIPT: &str =
    "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi";

/// One protocol frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Exit(i32),
    Close,
}

/// Clamp a requested terminal size into the accepted range.
pub fn clamp_size(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    (
        cols.unwrap_or(DEFAULT_COLS).clamp(MIN_COLS, MAX_COLS),
        rows.unwrap_or(DEFAULT_ROWS).clamp(MIN_ROWS, MAX_ROWS),
    )
}

/// Write one frame. Data larger than [`MAX_FRAME_PAYLOAD`] is split.
pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match frame {
        Frame::Data(bytes) => {
            for chunk in bytes.chunks(MAX_FRAME_PAYLOAD) {
                write_raw(writer, FRAME_DATA, chunk).await?;
            }
            if bytes.is_empty() {
                write_raw(writer, FRAME_DATA, &[]).await?;
            }
        }
        Frame::Resize { cols, rows } => {
            let mut payload = [0u8; 4];
            payload[..2].copy_from_slice(&cols.to_be_bytes());
            payload[2..].copy_from_slice(&rows.to_be_bytes());
            write_raw(writer, FRAME_RESIZE, &payload).await?;
        }
        Frame::Exit(code) => write_raw(writer, FRAME_EXIT, &code.to_be_bytes()).await?,
        Frame::Close => write_raw(writer, FRAME_CLOSE, &[]).await?,
    }
    writer.flush().await
}

async fn write_raw<W>(writer: &mut W, kind: u8, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await
}

/// Read one frame. `Ok(None)` is a clean end of stream between frames; a
/// truncated frame, an unknown kind or an oversized payload is an error.
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Frame>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; 5];
    let mut filled = 0;
    while filled < header.len() {
        let n = reader.read(&mut header[filled..]).await?;
        if n == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated frame header",
            ));
        }
        filled += n;
    }
    let kind = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(invalid("frame payload is too large"));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    match kind {
        FRAME_DATA => Ok(Some(Frame::Data(payload))),
        FRAME_RESIZE => {
            let bytes: [u8; 4] = payload
                .try_into()
                .map_err(|_| invalid("resize frame must carry 4 bytes"))?;
            Ok(Some(Frame::Resize {
                cols: u16::from_be_bytes([bytes[0], bytes[1]]),
                rows: u16::from_be_bytes([bytes[2], bytes[3]]),
            }))
        }
        FRAME_EXIT => {
            let bytes: [u8; 4] = payload
                .try_into()
                .map_err(|_| invalid("exit frame must carry 4 bytes"))?;
            Ok(Some(Frame::Exit(i32::from_be_bytes(bytes))))
        }
        FRAME_CLOSE if payload.is_empty() => Ok(Some(Frame::Close)),
        FRAME_CLOSE => Err(invalid("close frame carries no payload")),
        _ => Err(invalid("unknown frame kind")),
    }
}

/// Read one `\n` terminated line of at most `max` bytes from a buffered
/// reader, without consuming anything after the newline. `Ok(None)` is a
/// clean end of stream.
pub async fn read_line_capped<R>(reader: &mut R, max: usize) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut out = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            out.extend_from_slice(&buffer[..position]);
            reader.consume(position + 1);
            if out.len() > max {
                return Err(invalid("line exceeds the protocol size limit"));
            }
            return Ok(Some(out));
        }
        let consumed = buffer.len();
        out.extend_from_slice(buffer);
        reader.consume(consumed);
        if out.len() > max {
            return Err(invalid("line exceeds the protocol size limit"));
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

// ---------------------------------------------------------------------------
// Policy

/// Host paths whose bind mount makes a container equivalent to host root
/// access. A bind mount of one of these, of a folder inside one, or of a
/// parent folder (such as `/` or `/var`) refuses the terminal.
const SYSTEM_PATHS: &[&str] = &[
    "/etc", "/root", "/home", "/boot", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64",
    "/proc", "/sys", "/dev", "/run", "/var/run", "/var/lib", "/var/log",
];

/// Capabilities that let a process escape or administer the host.
const DANGEROUS_CAPS: &[&str] = &[
    "ALL",
    "SYS_ADMIN",
    "SYS_PTRACE",
    "SYS_MODULE",
    "SYS_RAWIO",
    "DAC_READ_SEARCH",
    "NET_ADMIN",
    "BPF",
    "PERFMON",
];

/// Whether a terminal may be opened into a container, from its parsed
/// inspect data ([`crate::docker::containers::parse_inspect_json`]).
///
/// A RAST admin must not become root on the host through a container, so
/// containers that are privileged, share host namespaces, have dangerous
/// capabilities or devices, or bind-mount system folders or the Docker socket
/// are refused. The reason is written for the person who asked.
pub fn terminal_policy(inspect: &Value) -> (bool, String) {
    let deny = |reason: &str| (false, reason.to_string());
    if inspect
        .get("privileged")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return deny(
            "This container is privileged, so a terminal would have full access to the server.",
        );
    }
    if str_field(inspect, "pidMode") == "host" {
        return deny("This container shares the server's processes.");
    }
    if str_field(inspect, "ipcMode") == "host" {
        return deny("This container shares the server's memory.");
    }
    if str_field(inspect, "networkMode") == "host" {
        return deny("This container shares the server's network.");
    }
    if inspect.get("devices").and_then(Value::as_u64).unwrap_or(0) > 0 {
        return deny("This container has access to server devices.");
    }
    let caps = inspect
        .get("capAdd")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for cap in caps.iter().filter_map(Value::as_str) {
        let cap = cap.trim_start_matches("CAP_").to_ascii_uppercase();
        if DANGEROUS_CAPS.contains(&cap.as_str()) {
            return deny("This container has extra privileges on the server.");
        }
    }
    let mounts = inspect
        .get("mounts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for mount in mounts {
        if str_field(mount, "type") != "bind" {
            continue;
        }
        let source = str_field(mount, "source").trim_end_matches('/');
        if source.is_empty() {
            return deny("This container mounts the server's root folder.");
        }
        if source.ends_with("docker.sock") || source.ends_with("containerd.sock") {
            return deny("This container can control Docker on the server.");
        }
        let inside = |path: &str| source == path || source.starts_with(&format!("{path}/"));
        let parent = |path: &str| path.starts_with(&format!("{source}/"));
        if SYSTEM_PATHS.iter().any(|path| inside(path) || parent(path)) {
            return deny("This container mounts a system folder of the server.");
        }
    }
    (true, String::new())
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frames_round_trip() {
        let frames = [
            Frame::Data(b"ls -la\r".to_vec()),
            Frame::Resize {
                cols: 132,
                rows: 43,
            },
            Frame::Exit(-1),
            Frame::Exit(130),
            Frame::Close,
        ];
        let mut buffer = Vec::new();
        for frame in &frames {
            write_frame(&mut buffer, frame).await.unwrap();
        }
        let mut reader = buffer.as_slice();
        for frame in &frames {
            assert_eq!(read_frame(&mut reader).await.unwrap().as_ref(), Some(frame));
        }
        assert_eq!(read_frame(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn large_data_is_split_into_bounded_frames() {
        let data = vec![b'x'; MAX_FRAME_PAYLOAD * 2 + 10];
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &Frame::Data(data.clone()))
            .await
            .unwrap();
        let mut reader = buffer.as_slice();
        let mut joined = Vec::new();
        let mut count = 0;
        while let Some(Frame::Data(chunk)) = read_frame(&mut reader).await.unwrap() {
            assert!(chunk.len() <= MAX_FRAME_PAYLOAD);
            joined.extend(chunk);
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(joined, data);
    }

    #[tokio::test]
    async fn malformed_frames_are_errors() {
        // Unknown kind.
        let mut reader: &[u8] = &[9, 0, 0, 0, 0];
        assert!(read_frame(&mut reader).await.is_err());
        // Oversized payload.
        let len = (MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes();
        let bytes = [FRAME_DATA, len[0], len[1], len[2], len[3]];
        let mut reader: &[u8] = &bytes;
        assert!(read_frame(&mut reader).await.is_err());
        // Truncated header and payload.
        let mut reader: &[u8] = &[FRAME_DATA, 0, 0];
        assert!(read_frame(&mut reader).await.is_err());
        let mut reader: &[u8] = &[FRAME_DATA, 0, 0, 0, 4, b'a'];
        assert!(read_frame(&mut reader).await.is_err());
        // Resize with the wrong length, close with a payload.
        let mut reader: &[u8] = &[FRAME_RESIZE, 0, 0, 0, 2, 0, 80];
        assert!(read_frame(&mut reader).await.is_err());
        let mut reader: &[u8] = &[FRAME_CLOSE, 0, 0, 0, 1, 0];
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn capped_lines_leave_the_rest_buffered() {
        let data: &[u8] = b"{\"ok\":true}\n\x01\x00\x00\x00\x02hi";
        let mut reader = tokio::io::BufReader::new(data);
        let line = read_line_capped(&mut reader, 64).await.unwrap().unwrap();
        assert_eq!(line, b"{\"ok\":true}");
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            Some(Frame::Data(b"hi".to_vec()))
        );
        let mut long = tokio::io::BufReader::new(&b"0123456789\n"[..]);
        assert!(read_line_capped(&mut long, 4).await.is_err());
        let mut empty = tokio::io::BufReader::new(&b""[..]);
        assert_eq!(read_line_capped(&mut empty, 4).await.unwrap(), None);
    }

    #[test]
    fn sizes_are_clamped() {
        assert_eq!(clamp_size(None, None), (DEFAULT_COLS, DEFAULT_ROWS));
        assert_eq!(clamp_size(Some(0), Some(0)), (MIN_COLS, MIN_ROWS));
        assert_eq!(clamp_size(Some(9999), Some(9999)), (MAX_COLS, MAX_ROWS));
        assert_eq!(clamp_size(Some(120), Some(40)), (120, 40));
    }

    #[test]
    fn terminal_is_refused_for_root_equivalent_containers() {
        let base = json!({"privileged": false, "pidMode": "", "ipcMode": "private", "networkMode": "bridge", "devices": 0, "capAdd": [], "mounts": [
            {"type": "bind", "source": "/srv/drive/data", "target": "/data"},
            {"type": "volume", "source": "/var/lib/docker/volumes/x/_data", "target": "/x"},
        ]});
        assert!(terminal_policy(&base).0);
        let with = |key: &str, value: Value| {
            let mut v = base.clone();
            v[key] = value;
            v
        };
        assert!(!terminal_policy(&with("privileged", json!(true))).0);
        assert!(!terminal_policy(&with("pidMode", json!("host"))).0);
        assert!(!terminal_policy(&with("ipcMode", json!("host"))).0);
        assert!(!terminal_policy(&with("networkMode", json!("host"))).0);
        assert!(!terminal_policy(&with("devices", json!(1))).0);
        assert!(!terminal_policy(&with("capAdd", json!(["CAP_SYS_ADMIN"]))).0);
        assert!(!terminal_policy(&with("capAdd", json!(["sys_ptrace"]))).0);
        assert!(terminal_policy(&with("capAdd", json!(["CHOWN"]))).0);
        assert!(terminal_policy(&with("networkMode", json!("container:abc"))).0);
        for source in [
            "/",
            "/etc",
            "/root/windows-share",
            "/home/someone",
            "/var",
            "/var/",
            "/var/run/docker.sock",
            "/srv/custom/docker.sock",
            "/run/containerd/containerd.sock",
            "/var/lib/docker",
            "/proc",
        ] {
            let policy = terminal_policy(&with(
                "mounts",
                json!([{"type": "bind", "source": source, "target": "/host"}]),
            ));
            assert!(!policy.0, "{source} should be refused");
            assert!(!policy.1.is_empty());
        }
        // Prefixes match whole path segments only.
        for source in ["/etcetera/data", "/srv", "/var/www", "/mnt/media"] {
            assert!(
                terminal_policy(&with(
                    "mounts",
                    json!([{"type": "bind", "source": source, "target": "/d"}])
                ))
                .0,
                "{source} should be allowed"
            );
        }
    }
}
