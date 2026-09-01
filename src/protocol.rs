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
    /// pipe to the child's fd 0, writes these bytes, then closes the pipe so
    /// the child sees EOF. When absent, fd 0 is `/dev/null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Maximum runtime in seconds. Zero means no timeout.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub timeout: u64,
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
    /// Requested maximum runtime in seconds. Zero means no timeout.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub timeout: u64,
    pub exit: i32,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
}

pub const ERROR_NOT_FOUND: &str = "not_found";
pub const ERROR_ABORTED: &str = "aborted";
pub const ERROR_TIMEOUT: &str = "timeout";

fn is_zero(value: &u64) -> bool {
    *value == 0
}

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
    Attach {
        /// Send raw PTY output, including ANSI sequences, in transcript events.
        #[serde(default)]
        ansi: bool,
    },
}

/// A completed transcript entry sent to an attached client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum HostEvent {
    Transcript { entry: TranscriptEntry },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timeout_defaults_to_zero() {
        let request: Request =
            serde_json::from_str(r#"{"whoami":"test","dir":"/tmp","exec":["true"]}"#).unwrap();
        assert_eq!(request.timeout, 0);
    }

    #[test]
    fn zero_timeout_is_omitted() {
        let request = Request {
            whoami: "test".into(),
            dir: "/tmp".into(),
            envs: BTreeMap::new(),
            exec: vec!["true".into()],
            stdin: None,
            timeout: 0,
        };
        let value = serde_json::to_value(request).unwrap();
        assert!(value.get("timeout").is_none());
    }

    #[test]
    fn attach_action_selects_raw_ansi_output() {
        let action = ClientAction::Attach { ansi: true };
        assert_eq!(
            serde_json::to_string(&action).unwrap(),
            r#"{"action":"attach","ansi":true}"#
        );
    }
}
