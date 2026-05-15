//! MCP gateway — L5 Orchestrator Facade.
//!
//! Claude/GPT/Gemini call this gateway via MCP tools. The gateway hides comb-level
//! placement and translates tool calls into the Honeycomb HTTP/NATS API.
//!
//! v1 tools: describe_cluster, run_subagent, estimate_cost.
//! Deferred: run_subagent_batch (fan-out), get_balance (requires Ledger).

pub mod tools;
pub mod client;
pub mod error;

pub use client::HoneycombClient;
pub use error::{GatewayError, GatewayResult};
pub use tools::{DescribeClusterResponse, RunSubagentRequest, RunSubagentResponse,
                EstimateCostRequest, EstimateCostResponse, McpTools};
