# CLAUDE.md — hive-mcp-gateway

## What this is

MCP (Model Context Protocol) gateway crate for HiveFabric. Exposes Honeycomb's dispatch capability as a set of tool-callable functions. Two surfaces:
1. **Library** (`hive-mcp-gateway` crate): `McpTools` trait + `HttpMcpTools` impl — used by `hive-tenant-gateway` to route `run_subagent` / `describe_cluster` / `estimate_cost` calls to Honeycomb.
2. **Binary** (`mcp_stdio`): stdio transport for direct MCP client use (e.g. Claude Desktop with the `mcp` server protocol).

## Key files

- `src/tools.rs` — `McpTools` trait + `HttpMcpTools` impl. All three tool implementations live here. This is the primary file to change.
- `src/client.rs` — `HoneycombClient`: thin HTTP wrapper around Honeycomb's REST API with `get_json` / `post_json` helpers.
- `src/error.rs` — `GatewayError` enum; maps HTTP status codes to typed errors.
- `src/bin/mcp_stdio.rs` — stdio transport entry point. Used when running as a standalone MCP server.

## How to run (library mode)

This crate is a dependency of `hive-tenant-gateway`. It doesn't have its own server port. Used via `HttpMcpTools::new(HoneycombClient::new(url, api_key))`.

## How to run (stdio MCP server)

```bash
HONEYCOMB_URL=http://localhost:8080 \
HONEYCOMB_API_KEY=dev-hive-key \
cargo run --bin mcp_stdio
```

The binary reads JSON-RPC messages from stdin and writes responses to stdout.

## How to test

```bash
cargo test -p hive-mcp-gateway   # 19 unit tests, no DB needed
```

## Architecture notes

- `run_subagent`: polls `GET /api/tasks/{id}` every 500ms until terminal status. Timeout configurable via `RunSubagentRequest.timeout_seconds` (default 60s).
- `describe_cluster`: fetches `GET /api/capabilities` and returns the live capability list. No caching.
- `estimate_cost`: pure calculation — no API call needed. Credits estimated from capability URN category (7B/70B/GPU/code/web) × input token count. Phase 2 will use real benchmark data.
- ACP envelope: `build_task_create_body` wraps tasks in `AcpEnvelope<TaskCreateRequest>` with a fresh W3C traceparent and the task_id as idempotency key.
- Sensitivity and jurisdiction fields are injected from the request (not computed here — that's the Forager's job in Honeycomb).

## What's not done

- Streaming results: `run_subagent` polls; it does not use the WebSocket stream. Phase 2 will switch to streaming for better latency on long tasks.
- Tool discovery: `describe_cluster` returns capability URNs but not pricing or SLA metadata. Phase 2 will enrich this.
- Budget enforcement: credits_budget is passed through to Honeycomb but not enforced by the gateway (Honeycomb checks it).
