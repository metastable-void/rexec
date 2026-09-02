//! Stdio MCP server that exposes the rexec host as MCP tools.
//!
//! Maintains a pool of ping-verified Unix socket connections to the host,
//! reusing idle connections and adding connections for overlapping requests.
//! The `whoami` identity is fixed at launch time via `--whoami` so a single MCP
//! session is one logical agent.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, WaitTimeoutResult};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::mcp_server::{McpServerOptions, ServerHandler, server_runtime};
use rust_mcp_sdk::schema::schema_utils::CallToolError;
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, RpcError, ServerCapabilities, ServerCapabilitiesTools,
    TextContent,
};
use rust_mcp_sdk::{McpServer, StdioTransport, ToMcpServerHandler, TransportOptions, tool_box};

use crate::client;
use crate::protocol::{ERROR_NOT_FOUND, Request};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const TOOL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HOST_NOT_FOUND: &str = "HOST NOT FOUND";

#[mcp_tool(
    name = "exec",
    description = "Run a command via the rexec host (fresh PTY, ANSI-stripped output). \
        Returns a JSON object with `exit`, `output`, and optional `error` fields. \
        Pass environment overrides in `env` (use `{}` when none are needed); \
        `clear_env` clears the inherited environment first. Provide `stdin` to \
        feed the child a UTF-8 buffer; set `timeout` to a maximum runtime in \
        seconds (zero disables it).",
    read_only_hint = false,
    destructive_hint = true,
    idempotent_hint = false,
    open_world_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct ExecTool {
    /// Working directory the host should chdir into before exec.
    pub dir: String,
    /// argv: `argv[0]` is the program (resolved via PATH), rest are arguments.
    /// Must be non-empty.
    pub argv: Vec<String>,
    /// Environment variable overrides. Pass an empty object when none are
    /// needed. Names and values are forwarded without restriction.
    pub env: BTreeMap<String, String>,
    /// Clear the inherited environment before applying `env` overrides.
    #[serde(default)]
    pub clear_env: bool,
    /// Optional UTF-8 bytes to feed the child's stdin. When provided the host
    /// attaches a pipe to fd 0 and closes it after writing, so the child sees
    /// a real EOF rather than blocking on the PTY slave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Maximum runtime in seconds. Zero (the default) disables the timeout.
    #[serde(default)]
    pub timeout: u64,
}

#[mcp_tool(
    name = "check_host",
    description = "Check whether a rexec host is running for this user. \
        Returns either \"HOST RUNNING\" or \"HOST NOT FOUND\".",
    read_only_hint = true,
    destructive_hint = false,
    idempotent_hint = true,
    open_world_hint = false
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema, Default)]
pub struct CheckHostTool {}

tool_box!(RexecTools, [ExecTool, CheckHostTool]);

#[derive(Clone)]
struct Handler {
    whoami: String,
    connector: HostConnector,
}

/// Owns a pool of persistent, ping-verified host connections. Sequential calls
/// reuse idle sockets; overlapping calls establish additional sockets so they
/// can execute concurrently. The worker reconnects forever when the pool is
/// empty because of a failed handshake or disconnect.
#[derive(Clone)]
struct HostConnector {
    shared: Arc<ReconnectState>,
}

struct ReconnectState {
    path: PathBuf,
    interval: Duration,
    connection: Mutex<ConnectionState>,
    changed: Condvar,
}

struct ConnectionState {
    idle: Vec<client::HostConnection>,
    active: usize,
    attempts: u64,
}

fn lock_connection(state: &ReconnectState) -> MutexGuard<'_, ConnectionState> {
    state
        .connection
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_change<'a>(
    shared: &'a ReconnectState,
    state: MutexGuard<'a, ConnectionState>,
    timeout: Duration,
) -> (MutexGuard<'a, ConnectionState>, WaitTimeoutResult) {
    shared
        .changed
        .wait_timeout(state, timeout)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ConnectionLease {
    connector: HostConnector,
    connection: Option<client::HostConnection>,
    reusable: bool,
}

impl ConnectionLease {
    fn ping(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.reusable = false;
        let result = self.connection.as_mut().unwrap().ping_with_timeout(timeout);
        if result.is_ok() {
            self.reusable = true;
        }
        result
    }

    fn execute_after_ping(
        &mut self,
        request: &Request,
    ) -> std::io::Result<crate::protocol::Response> {
        // If execution unwinds or returns an I/O error, never return a socket
        // with unknown framing state to the idle pool.
        self.reusable = false;
        let result = self
            .connection
            .as_mut()
            .unwrap()
            .execute_after_ping(request);
        if result.is_ok() {
            self.reusable = true;
        }
        result
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        let mut state = lock_connection(&self.connector.shared);
        state.active -= 1;
        if self.reusable {
            state.idle.push(self.connection.take().unwrap());
        }
        self.connector.shared.changed.notify_all();
    }
}

impl HostConnector {
    fn start(path: PathBuf) -> std::io::Result<Self> {
        Self::start_with_interval(path, RECONNECT_INTERVAL)
    }

    fn start_with_interval(path: PathBuf, interval: Duration) -> std::io::Result<Self> {
        let connector = Self::new(path, interval);
        let weak = Arc::downgrade(&connector.shared);
        Self::spawn_reconnect_worker(weak, interval)?;
        Ok(connector)
    }

    fn new(path: PathBuf, interval: Duration) -> Self {
        let shared = Arc::new(ReconnectState {
            path,
            interval,
            connection: Mutex::new(ConnectionState {
                idle: Vec::new(),
                active: 0,
                attempts: 0,
            }),
            changed: Condvar::new(),
        });
        Self { shared }
    }

    fn spawn_reconnect_worker(
        weak: std::sync::Weak<ReconnectState>,
        interval: Duration,
    ) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("rexec-mcp-reconnect".into())
            .spawn(move || {
                while let Some(shared) = weak.upgrade() {
                    let mut state = lock_connection(&shared);
                    state
                        .idle
                        .retain(|connection| !connection.is_disconnected());
                    if !state.idle.is_empty() || state.active != 0 {
                        let _ = wait_for_change(&shared, state, interval);
                        drop(shared);
                        continue;
                    }
                    drop(state);

                    let connection =
                        client::HostConnection::connect_with_timeout(&shared.path, interval);
                    let failed = connection.is_err();
                    let mut state = lock_connection(&shared);
                    state.attempts = state.attempts.wrapping_add(1);
                    if let Ok(connection) = connection {
                        state.idle.push(connection);
                    }
                    shared.changed.notify_all();
                    drop(state);
                    drop(shared);
                    if failed {
                        std::thread::sleep(interval);
                    }
                }
            })?;
        Ok(())
    }

    fn acquire(&self, timeout: Duration) -> Result<ConnectionLease, String> {
        let deadline = Instant::now() + timeout;
        let mut state = lock_connection(&self.shared);
        loop {
            while let Some(connection) = state.idle.pop() {
                if !connection.is_disconnected() {
                    state.active += 1;
                    return Ok(ConnectionLease {
                        connector: self.clone(),
                        connection: Some(connection),
                        reusable: true,
                    });
                }
            }

            // Never depend exclusively on the background worker. Every caller
            // repairs an empty pool itself, which prevents a live stdio server
            // from becoming permanently unusable if that worker is delayed or
            // lost. When existing sockets are busy, this also enables genuine
            // concurrent execution without touching those active connections.
            let now = Instant::now();
            if now >= deadline {
                return Err(HOST_NOT_FOUND.to_string());
            }
            let remaining = deadline.saturating_duration_since(now);
            let attempt_timeout = remaining.min(self.shared.interval);
            drop(state);
            let connection =
                client::HostConnection::connect_with_timeout(&self.shared.path, attempt_timeout);
            state = lock_connection(&self.shared);
            if let Ok(connection) = connection {
                state.active += 1;
                return Ok(ConnectionLease {
                    connector: self.clone(),
                    connection: Some(connection),
                    reusable: true,
                });
            }
            if !state.idle.is_empty() {
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HOST_NOT_FOUND.to_string());
            }
            let (next, _) =
                wait_for_change(&self.shared, state, remaining.min(self.shared.interval));
            state = next;
        }
    }

    fn acquire_for_command(&self, timeout: Duration) -> Result<ConnectionLease, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HOST_NOT_FOUND.to_string());
            }
            let mut connection = self.acquire(remaining)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(connection);
                return Err(HOST_NOT_FOUND.to_string());
            }
            if connection.ping(remaining.min(self.shared.interval)).is_ok() {
                return Ok(connection);
            }
            // Ping failed before the Request was sent. Dropping this lease
            // invalidates it, wakes the reconnect machinery, and makes a retry
            // safe: command execution cannot be duplicated at this stage.
            drop(connection);
        }
    }

    #[cfg(test)]
    fn attempt_count(&self) -> u64 {
        lock_connection(&self.shared).attempts
    }

    #[cfg(test)]
    fn without_background_worker(path: PathBuf, interval: Duration) -> Self {
        Self::new(path, interval)
    }
}

impl Handler {
    fn exec(&self, tool: ExecTool) -> Result<CallToolResult, CallToolError> {
        if tool.argv.is_empty() {
            return Err(CallToolError::from_message(
                "argv must be non-empty".to_string(),
            ));
        }
        let request = Request {
            whoami: self.whoami.clone(),
            dir: tool.dir,
            envs: tool.env,
            clear_env: tool.clear_env,
            exec: tool.argv,
            stdin: tool.stdin,
            timeout: tool.timeout,
        };

        // Only acquiring the host socket is bounded. Once the request is sent,
        // command execution is governed solely by the tool's `timeout` value.
        let mut connection = match self.connector.acquire_for_command(TOOL_CONNECT_TIMEOUT) {
            Ok(connection) => connection,
            Err(err) => {
                return Ok(
                    CallToolResult::text_content(vec![TextContent::from(err.to_string())])
                        .with_is_error(true),
                );
            }
        };
        let response = match connection.execute_after_ping(&request) {
            Ok(r) => r,
            Err(err) => {
                return Ok(
                    CallToolResult::text_content(vec![TextContent::from(err.to_string())])
                        .with_is_error(true),
                );
            }
        };

        let mut body = serde_json::Map::new();
        body.insert("exit".into(), serde_json::Value::from(response.exit));
        body.insert("output".into(), serde_json::Value::from(response.output));
        if let Some(err) = &response.error {
            body.insert("error".into(), serde_json::Value::from(err.clone()));
        }
        let text = serde_json::to_string(&body)
            .unwrap_or_else(|_| "{\"error\":\"serialize_failed\"}".to_string());

        let is_error = response.exit != 0 || response.error.as_deref() == Some(ERROR_NOT_FOUND);
        let mut result = CallToolResult::text_content(vec![TextContent::from(text)]);
        if is_error {
            result = result.with_is_error(true);
        }
        Ok(result)
    }

    fn check_host(&self, _tool: CheckHostTool) -> Result<CallToolResult, CallToolError> {
        let msg = match self.connector.acquire(TOOL_CONNECT_TIMEOUT) {
            Ok(_) => "HOST RUNNING",
            Err(_) => "HOST NOT FOUND",
        };
        Ok(CallToolResult::text_content(vec![TextContent::from(
            msg.to_string(),
        )]))
    }
}

#[async_trait]
impl ServerHandler for Handler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: RexecTools::tools(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        let tool = RexecTools::try_from(params).map_err(CallToolError::new)?;
        // Socket I/O and command completion are blocking. A dedicated blocking
        // task keeps the MCP transport responsive and lets independent calls
        // execute concurrently through separate pooled connections.
        let handler = self.clone();
        tokio::task::spawn_blocking(move || {
            match tool {
                RexecTools::ExecTool(t) => handler.exec(t),
                RexecTools::CheckHostTool(t) => handler.check_host(t),
            }
            .map_err(|err| err.to_string())
        })
        .await
        .map_err(|err| CallToolError::from_message(format!("tool task failed: {err}")))?
        .map_err(CallToolError::from_message)
    }
}

trait WithIsError {
    fn with_is_error(self, is_error: bool) -> Self;
}

impl WithIsError for CallToolResult {
    fn with_is_error(mut self, is_error: bool) -> Self {
        self.is_error = Some(is_error);
        self
    }
}

pub fn run(whoami: String) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("rexec: failed to start tokio runtime: {err}");
            return 127;
        }
    };

    runtime.block_on(async move {
        let server_details = InitializeResult {
            server_info: Implementation {
                name: "rexec".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: Some("rexec MCP".into()),
                description: Some(
                    "Run commands through a per-user rexec host. The host serialises \
                     output to a shared human-readable console and journals every run \
                     to a JSONL transcript."
                        .into(),
                ),
                icons: vec![],
                website_url: Some("https://github.com/metastable-void/rexec".into()),
            },
            capabilities: ServerCapabilities {
                tools: Some(ServerCapabilitiesTools { list_changed: None }),
                ..Default::default()
            },
            meta: None,
            instructions: Some(
                "Use `exec` to run commands; argv[0] is resolved via PATH. \
                 Output is the combined stdout+stderr with ANSI escapes stripped \
                 and CR normalised to LF. A non-zero `exit` field marks failure; \
                 128+N indicates a signal, 127 means not-found or spawn failure."
                    .into(),
            ),
            protocol_version: ProtocolVersion::V2025_11_25.into(),
        };

        let transport = match StdioTransport::new(TransportOptions::default()) {
            Ok(t) => t,
            Err(err) => {
                eprintln!("rexec: failed to create stdio transport: {err}");
                return 127;
            }
        };

        let connector = match HostConnector::start(crate::socket::socket_path()) {
            Ok(connector) => connector,
            Err(err) => {
                eprintln!("rexec: failed to start host reconnect loop: {err}");
                return 127;
            }
        };
        let handler = Handler { whoami, connector };
        let server = server_runtime::create_server(McpServerOptions {
            server_details,
            transport,
            handler: handler.to_mcp_server_handler(),
            task_store: None,
            client_task_store: None,
            message_observer: None,
        });

        if let Err(err) = server.start().await {
            eprintln!(
                "rexec: mcp server: {}",
                err.rpc_error_message().unwrap_or(&err.to_string())
            );
            return 127;
        }
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

    fn test_socket_path(label: &str) -> PathBuf {
        let sequence = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "rexec-mcp-{label}-{}-{sequence}.sock",
            std::process::id()
        ))
    }

    fn delayed_handshake_host(path: PathBuf, delay: Duration) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.as_bytes(), crate::protocol::PING_LINE);
            stream.write_all(b"{\"result\":\"pong\"}\n").unwrap();
            stream.flush().unwrap();
            line.clear();
            let _ = reader.read_line(&mut line);
        })
    }

    fn handshake_host_connections(path: PathBuf, count: usize) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
            let mut connections = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                connections.push(std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    assert_eq!(line.as_bytes(), crate::protocol::PING_LINE);
                    stream.write_all(b"{\"result\":\"pong\"}\n").unwrap();
                    stream.flush().unwrap();
                    line.clear();
                    let _ = reader.read_line(&mut line);
                }));
            }
            for connection in connections {
                connection.join().unwrap();
            }
        })
    }

    fn failing_command_host(path: PathBuf, count: usize) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.as_bytes(), crate::protocol::PING_LINE);
                stream.write_all(b"{\"result\":\"pong\"}\n").unwrap();
                stream.flush().unwrap();
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.as_bytes(), crate::protocol::PING_LINE);
                stream.write_all(b"{\"result\":\"pong\"}\n").unwrap();
                stream.flush().unwrap();
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert!(!line.is_empty());
                // Drop without a response to simulate a host-side disconnect.
            }
        })
    }

    fn test_request() -> Request {
        Request {
            whoami: "test".into(),
            dir: "/tmp".into(),
            envs: BTreeMap::new(),
            clear_env: false,
            exec: vec!["true".into()],
            stdin: None,
            timeout: 0,
        }
    }

    #[test]
    fn exec_timeout_defaults_to_zero() {
        let tool: ExecTool = serde_json::from_value(serde_json::json!({
            "dir": "/tmp",
            "argv": ["true"],
            "env": {}
        }))
        .unwrap();
        assert_eq!(tool.timeout, 0);
        assert!(!tool.clear_env);
    }

    #[test]
    fn exec_accepts_environment_object_and_clear_flag() {
        let tool: ExecTool = serde_json::from_value(serde_json::json!({
            "dir": "/tmp",
            "argv": ["true"],
            "env": {"PATH": "/custom/bin", "TOKEN": "content"},
            "clear_env": true
        }))
        .unwrap();
        assert_eq!(tool.env["PATH"], "/custom/bin");
        assert_eq!(tool.env["TOKEN"], "content");
        assert!(tool.clear_env);
    }

    #[test]
    fn exec_requires_environment_object() {
        assert!(
            serde_json::from_value::<ExecTool>(serde_json::json!({
                "dir": "/tmp",
                "argv": ["true"]
            }))
            .is_err()
        );
    }

    #[test]
    fn exec_schema_exposes_environment_controls_and_timeout() {
        let tools = serde_json::to_value(RexecTools::tools()).unwrap();
        let tools = tools.to_string();
        assert!(tools.contains("\"env\""));
        assert!(tools.contains("\"clear_env\""));
        assert!(tools.contains("\"timeout\""));
    }

    #[test]
    fn connector_retries_until_the_host_appears() {
        let path = test_socket_path("reconnect");
        let connector =
            HostConnector::start_with_interval(path.clone(), Duration::from_millis(25)).unwrap();
        let host = delayed_handshake_host(path.clone(), Duration::from_millis(60));

        let connection = connector
            .acquire(Duration::from_millis(600))
            .expect("connector should find a host that appears later");
        drop(connection);
        assert!(connector.attempt_count() >= 1);
        drop(connector);
        host.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn connector_bounds_a_tool_wait_but_keeps_retrying() {
        let path = test_socket_path("timeout");
        let connector =
            HostConnector::start_with_interval(path, Duration::from_millis(10)).unwrap();
        let started = Instant::now();
        assert!(matches!(
            connector.acquire(Duration::from_millis(65)),
            Err(err) if err == HOST_NOT_FOUND
        ));
        let attempts_at_timeout = connector.attempt_count();
        assert!(started.elapsed() >= Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(35));
        assert!(connector.attempt_count() > attempts_at_timeout);
    }

    #[test]
    fn long_running_command_does_not_trigger_reconnection() {
        let path = test_socket_path("in-use");
        let connector =
            HostConnector::start_with_interval(path.clone(), Duration::from_millis(20)).unwrap();
        let host = delayed_handshake_host(path.clone(), Duration::ZERO);
        let connection = connector.acquire(Duration::from_millis(500)).unwrap();
        // Let any connection attempt that began before the lease became active
        // finish; only attempts initiated while the command is active matter.
        std::thread::sleep(Duration::from_millis(60));
        let attempts_while_acquired = connector.attempt_count();

        std::thread::sleep(Duration::from_millis(90));
        assert_eq!(connector.attempt_count(), attempts_while_acquired);

        drop(connection);
        drop(connector);
        host.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn overlapping_calls_acquire_independent_connections() {
        let path = test_socket_path("concurrent");
        let host = handshake_host_connections(path.clone(), 2);
        let connector =
            HostConnector::start_with_interval(path.clone(), Duration::from_millis(20)).unwrap();

        let first = connector.acquire(Duration::from_millis(500)).unwrap();
        let second = connector.acquire(Duration::from_millis(500)).unwrap();
        let attempts_while_active = connector.attempt_count();
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(connector.attempt_count(), attempts_while_active);

        drop(first);
        drop(second);
        drop(connector);
        host.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn waiting_call_reuses_a_connection_returned_during_retry() {
        let path = test_socket_path("returned-during-retry");
        let host = delayed_handshake_host(path.clone(), Duration::ZERO);
        let connector =
            HostConnector::start_with_interval(path.clone(), Duration::from_millis(100)).unwrap();
        let first = connector.acquire(Duration::from_millis(500)).unwrap();

        let waiting_connector = connector.clone();
        let waiting =
            std::thread::spawn(move || waiting_connector.acquire(Duration::from_millis(500)));
        std::thread::sleep(Duration::from_millis(30));
        drop(first);
        let second = waiting.join().unwrap().unwrap();

        drop(second);
        drop(connector);
        host.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn repeated_request_disconnects_always_recover() {
        let path = test_socket_path("request-recovery");
        let host = failing_command_host(path.clone(), 4);
        // Deliberately omit the background worker: every call must remain able
        // to repair the pool by itself, so losing that worker cannot require an
        // MCP session restart.
        let connector =
            HostConnector::without_background_worker(path.clone(), Duration::from_millis(20));

        for _ in 0..4 {
            let mut connection = connector
                .acquire_for_command(Duration::from_millis(500))
                .unwrap();
            assert!(connection.execute_after_ping(&test_request()).is_err());
            drop(connection);
        }

        drop(connector);
        host.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn poisoned_pool_lock_does_not_strand_the_server() {
        let path = test_socket_path("poison-recovery");
        let connector =
            HostConnector::without_background_worker(path.clone(), Duration::from_millis(20));
        let shared = connector.shared.clone();
        let poisoner = std::thread::spawn(move || {
            let _state = shared.connection.lock().unwrap();
            panic!("deliberately poison the pool lock");
        });
        assert!(poisoner.join().is_err());

        let host = delayed_handshake_host(path.clone(), Duration::ZERO);
        let connection = connector.acquire(Duration::from_millis(500)).unwrap();
        drop(connection);
        drop(connector);
        host.join().unwrap();
        let _ = std::fs::remove_file(path);
    }
}
