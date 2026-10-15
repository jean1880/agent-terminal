//! Headless stdio MCP entry point. The app owns processes and authority; this bridge only
//! translates the MCP lifecycle and typed tool calls to the local delegation socket.
//!
//! Nothing from the capability environment, incoming arguments or IPC frames is logged.
#[path = "cross_agent_mcp/client.rs"]
mod client;
#[path = "cross_agent_mcp/stdio.rs"]
mod stdio;
#[cfg(test)]
#[path = "cross_agent_mcp/tests.rs"]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use agent_core::delegation::correlation::invocation_from_meta;
use agent_core::delegation::ipc::{Call, CallOutcome};
use agent_core::delegation::wire::{guard_output, parse_request, ToolRequest};
use agent_core::delegation::{ErrorCode, Limits, OperationId, ToolError, ToolName};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::{json, Value};

use client::Client;

/// Runs before GTK initialisation. Stdout is reserved exclusively for MCP frames.
pub(crate) fn run() -> Result<(), String> {
    let started = Instant::now();
    let config = client::Config::from_environment();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| "cannot start the delegation bridge runtime".to_owned())?;
    runtime.block_on(async {
        let bridge = Bridge::new(Client::start(config, started));
        let transport = stdio::BoundedStdio::new(bridge.cancellations.clone());
        bridge
            .serve(transport)
            .await
            .map_err(|_| "cannot initialise the delegation MCP service".to_owned())?
            .waiting()
            .await
            .map_err(|_| "the delegation MCP service ended unexpectedly".to_owned())?;
        Ok(())
    })
}

#[derive(Clone)]
struct Bridge {
    client: Client,
    cancellations: stdio::CancellationRegistry,
}

impl Bridge {
    fn new(client: Client) -> Self {
        Self {
            client,
            cancellations: stdio::CancellationRegistry::default(),
        }
    }
}

impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("agent-terminal", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let available = self.client.ready().await.is_ok();
        let names: &[ToolName] = if available {
            &ToolName::ALL
        } else {
            &agent_core::delegation::tools::DEGRADED_TOOLS
        };
        Ok(ListToolsResult::with_all_items(
            names.iter().copied().map(tool).collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let request_id = serde_json::to_string(&context.id)
            .map_err(|_| McpError::invalid_request("invalid request id", None))?;
        let cancellation = context.ct.clone();
        let _registration = self
            .cancellations
            .register(request_id, move || cancellation.cancel());
        let name = ToolName::parse(&request.name)
            .ok_or_else(|| McpError::invalid_params("unknown delegation tool", None))?;
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let parsed = match parse_request(name, &arguments, &Limits::default()) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(error_result(error).into()),
        };
        let timeout_ms = match &parsed {
            ToolRequest::Wait(wait) => wait.timeout_ms.unwrap_or(10_000).saturating_add(2_000),
            _ => 30_000,
        };
        // Correlation is provider-supplied metadata only, never inferred from the MCP id.
        let invocation = invocation_from_meta(&context.meta);
        let call = Call {
            id: self.client.next_id(),
            tool: name,
            args: arguments,
            timeout_ms,
            invocation,
        };
        let response = tokio::select! {
            response = self.client.call(call) => response,
            _ = context.ct.cancelled() => Err(ToolError::new(
                ErrorCode::DeadlineExceeded,
                "the MCP caller cancelled this call; accepted mutations are unchanged",
            )),
        };
        Ok(match response {
            Ok(CallOutcome::Ok(output)) => {
                let operation = output
                    .get("operation_id")
                    .and_then(Value::as_str)
                    .and_then(|id| OperationId::parse(id).ok());
                match guard_output(output, operation.as_ref()) {
                    Ok(output) => CallToolResult::structured(output),
                    Err(error) => {
                        eprintln!("agent-terminal: delegation response exceeded its output budget");
                        error_result(error)
                    }
                }
            }
            Ok(CallOutcome::Err(error)) | Err(error) => error_result(error),
        }
        .into())
    }
}

fn tool(name: ToolName) -> Tool {
    // P1 owns these schemas; the same parser validates what they document.
    let input = name.input_schema().as_object().cloned().unwrap_or_default();
    let output = name
        .output_schema()
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut tool = Tool::new(name.as_str(), name.description(), Arc::new(input));
    tool.output_schema = Some(Arc::new(output));
    tool
}

fn error_result(error: ToolError) -> CallToolResult {
    // Guard errors too: a corrupt or newer supervisor must not make this bridge emit an
    // oversized result. The guard affects the response, never an accepted operation.
    let operation = error
        .details
        .as_ref()
        .and_then(|details| details.get("operation_id"))
        .and_then(Value::as_str)
        .and_then(|id| OperationId::parse(id).ok());
    let output = json!({"error": error});
    let output = match guard_output(output, operation.as_ref()) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("agent-terminal: delegation error exceeded its output budget");
            json!({"error": error})
        }
    };
    CallToolResult::structured_error(output)
}

fn unavailable(reason: &str) -> ToolError {
    ToolError::new(ErrorCode::RuntimeUnavailable, reason)
}
