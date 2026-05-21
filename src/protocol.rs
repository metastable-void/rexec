use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub whoami: String,
    pub dir: String,
    #[serde(default)]
    pub envs: BTreeMap<String, String>,
    pub exec: Vec<String>,
    /// Bytes to feed to the child's stdin. When present, the host attaches a
    /// pipe to the child's fd 0 (instead of the PTY slave), writes these
    /// bytes, then closes the pipe so the child sees EOF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub exit: i32,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub whoami: String,
    pub dir: String,
    #[serde(default)]
    pub envs: BTreeMap<String, String>,
    pub exec: Vec<String>,
    pub exit: i32,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

pub const ERROR_NOT_FOUND: &str = "not_found";
pub const ERROR_ABORTED: &str = "aborted";

/// Control messages from client to host. Encoded as JSONL with the `"action"`
/// field as the discriminator.
///
/// `Ping` may appear as the first message on a fresh connection (instead of a
/// `Request`); the host replies with a `ControlResponse::Pong` and closes.
/// `Abort` may appear at any point after a `Request` and asks the host to
/// terminate the corresponding child.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ClientAction {
    Ping,
    Abort,
}

/// Reply to a control-only request (e.g. a `Ping`). The discriminator is the
/// `"result"` field so it can't be confused with a `Response` for an executed
/// command (which has `"exit"`/`"output"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "lowercase")]
pub enum ControlResponse {
    Pong,
}

pub const PING_LINE: &[u8] = b"{\"action\":\"ping\"}\n";
pub const ABORT_LINE: &[u8] = b"{\"action\":\"abort\"}\n";
