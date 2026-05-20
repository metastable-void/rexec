use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub whoami: String,
    pub dir: String,
    #[serde(default)]
    pub envs: BTreeMap<String, String>,
    pub exec: Vec<String>,
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
