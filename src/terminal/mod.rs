//! Terminal: administrator shells on the server itself.
//!
//! The web server runs as the unprivileged service account, so the shells
//! come from a separate root service, `rast-terminal.service` (`rast terminal
//! helper`), that the server owner turns on once with `sudo rast terminal
//! enable` ([`unit`]). Nothing gives a browser a root shell until then.
//!
//! The helper listens on a Unix socket ([`socket_path`]) that only the RAST
//! service account (and root) may use: the directory and socket belong to its
//! group and every connection's peer uid is checked. The web server decides
//! who is an administrator; the helper keeps each shell's owner and only lets
//! that account list, reattach or end it.
//!
//! Shells outlive their browser tab: closing the page or losing the network
//! only detaches the viewer, and the shell keeps running with its recent
//! output kept ([`SCROLLBACK`]) for the next attach. A shell ends when it
//! exits, when its tab is closed, after [`DETACHED_IDLE`] without a viewer or
//! output, when its owner stops being an administrator, or when the helper
//! stops.
//!
//! Wire format: one JSON request line ([`Request`]), one JSON response line;
//! `run` answers with the command's output once it ends;
//! `open` and `attach` then switch to the binary frames of
//! [`crate::docker::exec`] (data, resize, exit, close = end the shell,
//! detached = another window took the shell over). Ending the connection
//! without a close frame detaches.

pub mod helper;
pub mod unit;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixStream,
    },
};

use crate::docker::exec::read_line_capped;

/// Where the helper listens unless `RAST_TERMINAL_SOCKET` says otherwise.
pub const DEFAULT_SOCKET: &str = "/run/rast-terminal/helper.sock";
/// The account allowed to connect unless `RAST_TERMINAL_PEER_USER` says
/// otherwise (the account `rast.service` runs as).
pub const DEFAULT_PEER_USER: &str = "rast";
/// Shells open at the same time on this server.
pub const MAX_SESSIONS: usize = 8;
/// Recent output kept per shell and replayed on attach.
pub const SCROLLBACK: usize = 256 * 1024;
/// A shell without a viewer and without output for this long is ended.
pub const DETACHED_IDLE: Duration = Duration::from_secs(24 * 60 * 60);
/// Largest request or response line.
pub(crate) const MAX_LINE: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a `run` command may take.
pub const MAX_RUN_SECS: u32 = 600;
/// Longest `run` command line.
pub const MAX_RUN_COMMAND: usize = 32 * 1024;
/// Output kept per stream of a `run` command.
pub const RUN_OUTPUT: usize = 64 * 1024;
/// Largest `run` answer line (output escaped as JSON).
const MAX_RUN_LINE: usize = 1024 * 1024;

/// The helper's socket (`RAST_TERMINAL_SOCKET`, for tests and development).
pub fn socket_path() -> PathBuf {
    std::env::var_os("RAST_TERMINAL_SOCKET")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

/// A request from the web server (or `rast terminal status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// Is the helper running? Answers `{version, sessions, max}`.
    Status,
    /// The account's shells.
    List { user: i64 },
    /// Start a shell for the account and attach to it.
    Open {
        user: i64,
        #[serde(rename = "userName")]
        user_name: String,
        cols: u16,
        rows: u16,
    },
    /// Attach to one of the account's shells (its recent output first).
    Attach {
        user: i64,
        session: String,
        cols: u16,
        rows: u16,
    },
    /// End one of the account's shells.
    Kill { user: i64, session: String },
    /// End every shell of the account (it lost access) and clean up after
    /// them (sandboxes: remove the account's container).
    KillUser { user: i64 },
    /// Accounts that have shells open: `{owners: [id, …]}`.
    Owners,
    /// Get ready to open shells for the account without waiting (sandboxes:
    /// build the image). Answers like `status`.
    Prepare {
        user: i64,
        #[serde(rename = "userName")]
        user_name: String,
    },
    /// Run one command for the account in a fresh login shell without a
    /// terminal (the AI Agent's shell tool) and answer [`RunOutput`] when it
    /// ends; it is stopped after `timeoutSecs`.
    Run {
        user: i64,
        #[serde(rename = "userName")]
        user_name: String,
        command: String,
        #[serde(rename = "timeoutSecs")]
        timeout_secs: u32,
    },
}

/// What a `run` command printed and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunOutput {
    /// `None` when it was stopped (timed out or killed by a signal).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Output beyond [`RUN_OUTPUT`] per stream was left out.
    pub truncated: bool,
    pub timed_out: bool,
}

/// One shell, as listed to its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    /// 1, 2, 3 … per account, for tab names.
    pub number: u32,
    /// Unix milliseconds.
    pub created_at: i64,
    /// A browser window shows it right now.
    pub attached: bool,
    /// The program in the foreground (`bash`, `htop`, …), when known.
    #[serde(default)]
    pub program: String,
}

/// Session ids are 16 lowercase hex characters.
pub fn valid_session_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Why the helper could not be used.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// The helper is not installed or not running.
    #[error("the terminal helper is not running (turn it on with `sudo rast terminal enable`)")]
    Off,
    /// The helper refused the request.
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("terminal helper: {0}")]
    Io(String),
}

/// An attached shell: frames flow over `reader`/`writer`.
pub struct HostSession {
    pub info: SessionInfo,
    pub reader: BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
}

/// The web server's side of a helper socket (the root helper's, or the
/// Docker worker's for sandboxes).
#[derive(Debug, Clone)]
pub struct TerminalClient {
    socket: PathBuf,
    /// How long `open` may take to answer (starting a sandbox container).
    open_timeout: Duration,
}

impl Default for TerminalClient {
    fn default() -> Self {
        Self::new(socket_path())
    }
}

impl TerminalClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            open_timeout: RESPONSE_TIMEOUT,
        }
    }

    /// Wait up to `timeout` for `open` to answer.
    pub fn with_open_timeout(mut self, timeout: Duration) -> Self {
        self.open_timeout = timeout;
        self
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn request(
        &self,
        request: &Request,
    ) -> Result<(Value, BufReader<OwnedReadHalf>, OwnedWriteHalf), TerminalError> {
        let stream =
            match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket)).await {
                Err(_) => return Err(TerminalError::Io("connecting timed out".into())),
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Err(TerminalError::Off)
                }
                Ok(Err(error)) => return Err(TerminalError::Io(error.to_string())),
                Ok(Ok(stream)) => stream,
            };
        let (read, mut write) = stream.into_split();
        let mut line = serde_json::to_vec(request).map_err(|e| TerminalError::Io(e.to_string()))?;
        line.push(b'\n');
        write
            .write_all(&line)
            .await
            .map_err(|e| TerminalError::Io(e.to_string()))?;
        let mut reader = BufReader::new(read);
        let (wait, cap) = match request {
            Request::Open { .. } => (self.open_timeout, MAX_LINE),
            // Starting a sandbox, then the command itself.
            Request::Run { timeout_secs, .. } => (
                self.open_timeout + Duration::from_secs(u64::from(*timeout_secs) + 15),
                MAX_RUN_LINE,
            ),
            _ => (RESPONSE_TIMEOUT, MAX_LINE),
        };
        let answer = tokio::time::timeout(wait, read_line_capped(&mut reader, cap))
            .await
            .map_err(|_| TerminalError::Io("no answer".into()))?
            .map_err(|e| TerminalError::Io(e.to_string()))?
            .ok_or_else(|| TerminalError::Io("the helper closed the connection".into()))?;
        let value: Value =
            serde_json::from_slice(&answer).map_err(|e| TerminalError::Io(e.to_string()))?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(TerminalError::Refused {
                code: value
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                message: value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the terminal helper refused the request")
                    .to_string(),
            });
        }
        Ok((value, reader, write))
    }

    /// `{version, sessions, max}` when the helper runs.
    pub async fn status(&self) -> Result<Value, TerminalError> {
        Ok(self.request(&Request::Status).await?.0)
    }

    pub async fn list(&self, user: i64) -> Result<Vec<SessionInfo>, TerminalError> {
        let (value, ..) = self.request(&Request::List { user }).await?;
        serde_json::from_value(value.get("sessions").cloned().unwrap_or(Value::Null))
            .map_err(|e| TerminalError::Io(e.to_string()))
    }

    pub async fn kill(&self, user: i64, session: &str) -> Result<(), TerminalError> {
        self.request(&Request::Kill {
            user,
            session: session.to_string(),
        })
        .await
        .map(|_| ())
    }

    /// End every shell of `user`; returns how many were open.
    pub async fn kill_user(&self, user: i64) -> Result<u64, TerminalError> {
        let (value, ..) = self.request(&Request::KillUser { user }).await?;
        Ok(value.get("killed").and_then(Value::as_u64).unwrap_or(0))
    }

    /// Accounts that have shells open.
    pub async fn owners(&self) -> Result<Vec<i64>, TerminalError> {
        let (value, ..) = self.request(&Request::Owners).await?;
        serde_json::from_value(value.get("owners").cloned().unwrap_or(Value::Null))
            .map_err(|e| TerminalError::Io(e.to_string()))
    }

    /// Get ready to open shells for `user` (answers like `status`).
    pub async fn prepare(&self, user: i64, user_name: &str) -> Result<Value, TerminalError> {
        Ok(self
            .request(&Request::Prepare {
                user,
                user_name: user_name.to_string(),
            })
            .await?
            .0)
    }

    /// Run `command` for `user` (see [`Request::Run`]).
    pub async fn run(
        &self,
        user: i64,
        user_name: &str,
        command: &str,
        timeout_secs: u32,
    ) -> Result<RunOutput, TerminalError> {
        let (value, ..) = self
            .request(&Request::Run {
                user,
                user_name: user_name.to_string(),
                command: command.to_string(),
                timeout_secs: timeout_secs.clamp(1, MAX_RUN_SECS),
            })
            .await?;
        serde_json::from_value(value).map_err(|e| TerminalError::Io(e.to_string()))
    }

    pub async fn open(
        &self,
        user: i64,
        user_name: &str,
        cols: u16,
        rows: u16,
    ) -> Result<HostSession, TerminalError> {
        self.session(&Request::Open {
            user,
            user_name: user_name.to_string(),
            cols,
            rows,
        })
        .await
    }

    pub async fn attach(
        &self,
        user: i64,
        session: &str,
        cols: u16,
        rows: u16,
    ) -> Result<HostSession, TerminalError> {
        self.session(&Request::Attach {
            user,
            session: session.to_string(),
            cols,
            rows,
        })
        .await
    }

    async fn session(&self, request: &Request) -> Result<HostSession, TerminalError> {
        let (value, reader, writer) = self.request(request).await?;
        let info = serde_json::from_value(value.get("session").cloned().unwrap_or(Value::Null))
            .map_err(|e| TerminalError::Io(e.to_string()))?;
        Ok(HostSession {
            info,
            reader,
            writer,
        })
    }
}

/// End every shell of an account that is no longer an administrator (role
/// change, deactivation, deletion). Best effort: nothing to do when the
/// helper is off.
pub async fn end_shells_of(user: i64) {
    match TerminalClient::default().kill_user(user).await {
        Ok(0) | Err(TerminalError::Off) => {}
        Ok(killed) => tracing::info!(
            user,
            killed,
            "ended the terminals of a former administrator"
        ),
        Err(error) => tracing::warn!(user, error = %error, "could not end the account's terminals"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_strict() {
        let open: Request = serde_json::from_str(
            r#"{"op":"open","user":1,"userName":"admin","cols":80,"rows":24}"#,
        )
        .unwrap();
        assert_eq!(
            open,
            Request::Open {
                user: 1,
                user_name: "admin".into(),
                cols: 80,
                rows: 24
            }
        );
        let kill: Request = serde_json::from_str(r#"{"op":"kill-user","user":7}"#).unwrap();
        assert_eq!(kill, Request::KillUser { user: 7 });
        for bad in [
            r#"{"op":"exec","cmd":"id"}"#,
            r#"{"op":"open","user":1,"userName":"a","cols":80,"rows":24,"command":"id"}"#,
            r#"{"op":"attach","user":1,"cols":80,"rows":24}"#,
            r#"{"op":"list"}"#,
            r#"{"user":1}"#,
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn session_ids_are_checked() {
        assert!(valid_session_id("0123456789abcdef"));
        for bad in [
            "",
            "0123456789ABCDEF",
            "0123456789abcde",
            "../../etc/passwd",
            "0123456789abcdefg",
        ] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }
}
