# hive-mcp-gateway

The L5 Orchestrator Facade for HiveFabric. This crate exposes Honeycomb
capabilities to LLM orchestrators (Claude, GPT, Gemini) via the
[Model Context Protocol](https://modelcontextprotocol.io/).

## What this crate is

`hive-mcp-gateway` is the *adapter* layer that translates MCP tool calls into
calls against the Honeycomb control plane. v1 targets Honeycomb's HTTP REST
API; Phase 2 will add a NATS-backed alternative behind the same trait so the
gateway can be co-located with comb-level brokers.

> **Note:** the actual MCP wire-protocol layer (stdio / JSON-RPC server) is
> **deferred**. This scaffold ships the adapter — `McpTools` trait,
> request/response types, and an HTTP-backed implementation. An MCP server
> harness comes next, likely via the [`rmcp`](https://crates.io/crates/rmcp)
> crate or hand-rolled over stdio.

## v1 tools

| Tool | Purpose | Status |
| --- | --- | --- |
| `describe_cluster` | List capabilities, online nodes, latency. | Stub — needs Honeycomb `/api/capabilities` (Batch 2.2). |
| `run_subagent` | Submit a single capability invocation and poll until terminal. | Wired against existing `/api/tasks/create` + `/api/tasks/{id}`. |
| `estimate_cost` | Estimate credit cost for an input size. | Stub — needs Honeycomb `/api/costs/estimate` (Batch 2.2). |

Stubbed tools currently return `GatewayError::Unsupported` and will be
unblocked once URN-native routing lands in Batch 2.2.

## Phase 2 plan

- `run_subagent_batch` — fan-out to multiple capabilities, gather results.
- `get_balance` — read tenant credit balance once the Ledger service exists.
- NATS-backed transport so the gateway can sit beside a comb broker instead
  of going through the central HTTP API.

## Wiring

```rust
use hive_mcp_gateway::{HoneycombClient, McpTools};
use hive_mcp_gateway::tools::HttpMcpTools;

let client = HoneycombClient::new("http://localhost:8080", None);
let tools = HttpMcpTools::new(client);
let cluster = tools.describe_cluster().await?;
```

## Quickstart

```bash
# from honeycomb/mcp-gateway
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
```

To exercise `run_subagent` against a local Honeycomb, start the
`honeycomb/service` crate first and point `HoneycombClient::new` at its bind
address.

## Layout

This crate stands alone — it is **not** a member of any workspace. The sibling
`honeycomb/service/` crate has its own `Cargo.toml`; the two are independent
peer projects under the `honeycomb/` directory.
