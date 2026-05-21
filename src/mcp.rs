//! Stdio MCP server that exposes the rexec host as MCP tools.
//!
//! Behaves like any other rexec client: each tool call opens a fresh Unix
//! socket connection to the host, sends a `Request`, and returns the host's
//! `Response` as MCP tool output. The `whoami` identity is fixed at launch
//! time via `--whoami` so a single MCP session is one logical agent.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::mcp_server::{McpServerOptions, ServerHandler, server_runtime};
use rust_mcp_sdk::schema::schema_utils::CallToolError;
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, RpcError, ServerCapabilities,
    ServerCapabilitiesTools, TextContent,
};
use rust_mcp_sdk::{
    McpServer, StdioTransport, ToMcpServerHandler, TransportOptions, tool_box,
};

use crate::client;
use crate::protocol::{ERROR_NOT_FOUND, Request};

#[mcp_tool(
    name = "exec",
    description = "Run a command via the rexec host (fresh PTY, ANSI-stripped output). \
        Returns a JSON object with `exit`, `output`, and optional `error` fields. \
        Set `read_stdin` and provide `stdin` to feed the child a UTF-8 buffer.",
    read_only_hint = false,
    destructive_hint = true,
    idempotent_hint = false,
    open_world_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct ExecTool {
    /// Working directory the host should chdir into before exec.
    pub dir: String,
    /// argv: argv[0] is the program (resolved via PATH), rest are arguments.
    /// Must be non-empty.
    pub argv: Vec<String>,
    /// Extra environment variables, each as "VAR=VAL". Added to (not replacing)
    /// the host's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envs: Option<Vec<String>>,
    /// Optional UTF-8 bytes to feed the child's stdin. When provided the host
    /// attaches a pipe to fd 0 and closes it after writing, so the child sees
    /// a real EOF rather than blocking on the PTY slave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
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

struct Handler {
    whoami: String,
}

impl Handler {
    fn exec(&self, tool: ExecTool) -> Result<CallToolResult, CallToolError> {
        if tool.argv.is_empty() {
            return Err(CallToolError::from_message(
                "argv must be non-empty".to_string(),
            ));
        }
        let mut envs = BTreeMap::new();
        for entry in tool.envs.unwrap_or_default() {
            let (k, v) = entry.split_once('=').ok_or_else(|| {
                CallToolError::from_message(format!(
                    "envs entry must be VAR=value, got: {entry}"
                ))
            })?;
            if k.is_empty() {
                return Err(CallToolError::from_message(format!(
                    "envs entry has empty name: {entry}"
                )));
            }
            envs.insert(k.to_string(), v.to_string());
        }
        let request = Request {
            whoami: self.whoami.clone(),
            dir: tool.dir,
            envs,
            exec: tool.argv,
            stdin: tool.stdin,
        };

        let response = match client::exec_blocking(&request) {
            Ok(r) => r,
            Err(err) => {
                return Ok(CallToolResult::text_content(vec![TextContent::from(err)])
                    .with_is_error(true));
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
        let msg = match std::os::unix::net::UnixStream::connect(crate::socket::socket_path()) {
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
        // Each rexec call is a blocking Unix-socket round-trip; keep it off the
        // tokio worker thread so the MCP transport stays responsive while a
        // command is running.
        tokio::task::block_in_place(|| match tool {
            RexecTools::ExecTool(t) => self.exec(t),
            RexecTools::CheckHostTool(t) => self.check_host(t),
        })
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

        let handler = Handler { whoami };
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
