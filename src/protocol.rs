use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 256 * 1024;
pub const MAX_OUTPUT: usize = 1024 * 1024;
pub const MAX_READ: usize = 32 * 1024;
pub const MAX_JOBS: usize = 16;
pub const MAX_RUNNING: usize = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub op: Operation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Info,
    Exec(Exec),
    Read { job_id: String, #[serde(default)] cursor: u64 },
    Cancel { job_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exec {
    pub expected_incarnation: String,
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}
fn default_timeout() -> u64 { 60_000 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub id: String,
    pub result: Option<Value>,
    pub error: Option<Fault>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fault { pub code: String, pub message: String }
impl Reply {
    pub fn ok(id: &str, result: impl Serialize) -> Self {
        Self { id: id.into(), result: Some(serde_json::to_value(result).expect("serializable result")), error: None }
    }
    pub fn err(id: &str, code: &str, message: impl Into<String>) -> Self {
        Self { id: id.into(), result: None, error: Some(Fault { code: code.into(), message: message.into() }) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Packet {
    Request { version: u32, request: Request },
    Reply { version: u32, reply: Reply },
    Lease { version: u32 },
    RelayError { code: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetInfo {
    pub session_id: String,
    pub target_id: String,
    pub incarnation: String,
    pub name: String,
    pub os: String,
    pub arch: String,
    pub uid: u32,
    pub gid: u32,
    pub cwd: String,
    pub expires_at: u64,
    pub protocol: u32,
    pub approval: String,
    pub end_to_end_encrypted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Output {
    pub seq: u64,
    pub stream: String,
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobView {
    pub target_id: String,
    pub incarnation: String,
    pub job_id: String,
    pub pid: u32,
    pub pgid: u32,
    pub process_start_ticks: Option<u64>,
    pub state: String,
    pub exit_code: Option<i32>,
    pub termination_reason: Option<String>,
    pub events: Vec<Output>,
    pub next_cursor: u64,
    pub output_truncated: bool,
    pub dropped_before_cursor: Option<u64>,
    pub output_encoding: String,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
