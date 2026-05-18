//! Hand-rolled MCP stdio JSON-RPC 2.0 server.
//!
//! Speaks newline-delimited JSON-RPC over stdin/stdout, the transport Claude Desktop
//! and the Anthropic SDK's MCP client expect. Logs to stderr only — stdout is reserved
//! for protocol bytes.
//!
//! Implements the minimum useful surface: `initialize`, `tools/list`, `tools/call`.
//! `notifications/*` arriving from the client are acknowledged but no-op'd.
//!
//! Configure via env:
//!   HONEYCOMB_URL      base URL of the Honeycomb control plane (default
//!                      http://localhost:8080)
//!   HONEYCOMB_API_KEY  optional API key forwarded as `x-api-key` (default unset)
//!
//! Claude Desktop config example (path varies by OS — see anthropic.com/mcp):
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "hivefabric": {
//!       "command": "/path/to/mcp-stdio",
//!       "env": {
//!         "HONEYCOMB_URL": "http://localhost:8080",
//!         "HONEYCOMB_API_KEY": "dev-hive-key"
//!       }
//!     }
//!   }
//! }
//! ```

use hive_mcp_gateway::{
    EstimateCostRequest, GatewayError, HoneycombClient, McpTools, RunSubagentRequest,
};
use hive_mcp_gateway::tools::HttpMcpTools;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// MCP / JSON-RPC 2.0 protocol version we advertise. The MCP spec is evolving;
/// 2024-11-05 was the published baseline. Newer Claude Desktop builds accept
/// any string here, then negotiate.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[tokio::main]
async fn main() {
    // Logs to stderr; stdout is reserved for protocol bytes.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let base_url =
        std::env::var("HONEYCOMB_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let api_key = std::env::var("HONEYCOMB_API_KEY").ok();
    tracing::info!(base_url = %base_url, "mcp-stdio starting");

    let client = HoneycombClient::new(base_url, api_key);
    let tools = HttpMcpTools::new(client);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "stdin read error");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = handle_message(trimmed, &tools).await;
        if let Some(response) = response {
            let serialized = serde_json::to_string(&response).unwrap_or_else(|_| {
                json!({"jsonrpc":"2.0","error":{"code":-32603,"message":"internal serialize"}})
                    .to_string()
            });
            if let Err(e) = writeln!(stdout, "{serialized}") {
                tracing::error!(error = %e, "stdout write error");
                break;
            }
            if let Err(e) = stdout.flush() {
                tracing::error!(error = %e, "stdout flush error");
                break;
            }
        }
    }
    tracing::info!("mcp-stdio shutting down");
}

/// Returns `None` for notifications (no response expected).
async fn handle_message<T: McpTools>(line: &str, tools: &T) -> Option<Value> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                -32700,
                format!("parse error: {e}"),
            ));
        }
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (per JSON-RPC 2.0): no `id` field — we do not respond.
    let is_notification = req.get("id").is_none();

    let outcome = match method {
        "initialize" => Ok(handle_initialize()),
        "tools/list" => Ok(handle_tools_list()),
        "tools/call" => handle_tools_call(&params, tools).await,
        "notifications/initialized" | "notifications/cancelled" => {
            // Acknowledge silently.
            return None;
        }
        "ping" => Ok(json!({})),
        _ => Err(GatewayError::Invalid(format!("unknown method: {method}"))),
    };

    if is_notification {
        return None;
    }

    Some(match outcome {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
        Err(GatewayError::Unsupported(msg)) => error_response(id, -32601, msg.to_string()),
        Err(GatewayError::Invalid(msg)) => error_response(id, -32602, msg),
        Err(GatewayError::TaskTimeout { seconds }) => {
            error_response(id, -32000, format!("task timeout after {seconds}s"))
        }
        Err(GatewayError::TaskFailed(msg)) => {
            error_response(id, -32000, format!("task failed: {msg}"))
        }
        Err(GatewayError::ControlPlane(msg)) => {
            error_response(id, -32001, format!("control-plane: {msg}"))
        }
        Err(GatewayError::Io(e)) => error_response(id, -32603, format!("io: {e}")),
    })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "hive-mcp-gateway",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn handle_tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "describe_cluster",
                "description": "List the capabilities (workloads) HiveFabric can serve, with cost estimates and live worker counts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "run_subagent",
                "description": "Run a generic-inference task on the HiveFabric network. Pick a model (by model_id like \"qwen2.5:0.5b\" OR by full capability_urn like \"oasf://commons/inference/qwen2.5-0.5b/v1\") and send a prompt. The 'what' (classify, summarise, extract, rerank, …) lives in the prompt itself — there are no special-case workload schemas. Returns the model's response synchronously after the task completes or fails.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "model_id": {
                            "type": "string",
                            "description": "Canonical model identifier, e.g. \"qwen2.5:0.5b\", \"gemma:2b\", \"llama3.2:3b\". Pick one of model_id or capability_urn."
                        },
                        "capability_urn": {
                            "type": "string",
                            "description": "Full capability URN, e.g. \"oasf://commons/inference/qwen2.5-0.5b/v1\". Pick one of model_id or capability_urn. If both are given, capability_urn wins."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The instruction. Encodes the workload. Examples: \"Classify: 'great game!' as positive | negative.\" or \"Summarise in one sentence: <text>\" or \"Extract entities from: <text>\"."
                        },
                        "profile": {
                            "type": "string",
                            "description": "LLM profile name on the comb. Defaults to \"default\".",
                            "default": "default"
                        },
                        "timeout_seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "default": 60
                        }
                    },
                    "required": ["prompt"]
                }
            },
            {
                "name": "estimate_cost",
                "description": "Pre-execution cost estimate for a HiveFabric capability call.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "capability_urn": { "type": "string" },
                        "input_size_tokens": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["capability_urn", "input_size_tokens"]
                }
            }
        ]
    })
}

async fn handle_tools_call<T: McpTools>(
    params: &Value,
    tools: &T,
) -> Result<Value, GatewayError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::Invalid("missing tool name".into()))?;
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    match name {
        "describe_cluster" => {
            let resp = tools.describe_cluster().await?;
            Ok(wrap_tool_result(json!(resp)))
        }
        "run_subagent" => {
            let req: RunSubagentRequest = serde_json::from_value(args)
                .map_err(|e| GatewayError::Invalid(format!("run_subagent args: {e}")))?;
            let resp = tools.run_subagent(req).await?;
            Ok(wrap_tool_result(json!(resp)))
        }
        "estimate_cost" => {
            let req: EstimateCostRequest = serde_json::from_value(args)
                .map_err(|e| GatewayError::Invalid(format!("estimate_cost args: {e}")))?;
            let resp = tools.estimate_cost(req).await?;
            Ok(wrap_tool_result(json!(resp)))
        }
        other => Err(GatewayError::Invalid(format!("unknown tool: {other}"))),
    }
}

/// MCP `tools/call` results are wrapped in `{ content: [{type: "text", text: "..."}] }`.
fn wrap_tool_result(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [
            { "type": "text", "text": text }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubTools;

    #[async_trait::async_trait]
    impl McpTools for StubTools {
        async fn describe_cluster(
            &self,
        ) -> Result<hive_mcp_gateway::DescribeClusterResponse, GatewayError> {
            Err(GatewayError::Unsupported("describe_cluster"))
        }
        async fn run_subagent(
            &self,
            _req: RunSubagentRequest,
        ) -> Result<hive_mcp_gateway::RunSubagentResponse, GatewayError> {
            Err(GatewayError::Unsupported("run_subagent"))
        }
        async fn estimate_cost(
            &self,
            _req: EstimateCostRequest,
        ) -> Result<hive_mcp_gateway::EstimateCostResponse, GatewayError> {
            Err(GatewayError::Unsupported("estimate_cost"))
        }
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        })
        .to_string();
        let resp = handle_message(&req, &StubTools).await.expect("response");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "hive-mcp-gateway");
        assert_eq!(resp["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn tools_list_returns_three_tools() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        })
        .to_string();
        let resp = handle_message(&req, &StubTools).await.expect("response");
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"describe_cluster"));
        assert!(names.contains(&"run_subagent"));
        assert!(names.contains(&"estimate_cost"));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "totally/unknown"
        })
        .to_string();
        let resp = handle_message(&req, &StubTools).await.expect("response");
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn notification_returns_none() {
        let req = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })
        .to_string();
        let resp = handle_message(&req, &StubTools).await;
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let resp = handle_message("{ not json", &StubTools).await.expect("response");
        assert_eq!(resp["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn run_subagent_without_args_returns_invalid() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "run_subagent" }
        })
        .to_string();
        let resp = handle_message(&req, &StubTools).await.expect("response");
        assert_eq!(resp["error"]["code"], -32602);
    }
}
