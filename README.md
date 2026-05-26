# hive-mcp-gateway

MCP (Model Context Protocol) gateway crate for HiveFabric. Exposes Honeycomb's dispatch capability as tool-callable functions. Used in two modes:

1. **Library** — consumed by `hive-tenant-gateway` to back `describe_cluster`, `run_subagent`, and `estimate_cost` tool calls.
2. **Binary** (`mcp_stdio`) — standalone stdio MCP server for direct use with Claude Desktop or any MCP-compatible client.

---

## Tools

| Tool | Description |
|---|---|
| `describe_cluster` | Lists capability URNs advertised by online combs. |
| `run_subagent` | Submits a task to Honeycomb and polls until completion. Returns output + status. |
| `estimate_cost` | Estimates credits for a task based on the capability URN category and token count. |

---

## How to use (library mode)

```rust
use hive_mcp_gateway::{HttpMcpTools, HoneycombClient, tools::McpTools};

let client = HoneycombClient::new("http://localhost:8080", Some("api-key".into()));
let tools = HttpMcpTools::new(client);
let cluster = tools.describe_cluster().await?;
```

## How to run (stdio MCP server)

```bash
HONEYCOMB_URL=http://localhost:8080 \
HONEYCOMB_API_KEY=dev-hive-key \
cargo run --bin mcp_stdio
```

## How to test

```bash
cargo test -p hive-mcp-gateway   # 19+ unit tests, no live server needed
```

---

## Architecture notes

- `run_subagent` polls `GET /api/tasks/{id}` every 500ms until terminal status. No WebSocket streaming yet (Phase 2).
- Task bodies are wrapped in ACP envelopes with W3C traceparent and idempotency keys.
- `estimate_cost` is a pure calculation — no API call. Tiers: tiny (135M-360M) = 5 credits/1k, small (0.5B-3B) = 20 credits/1k, large (7B-14B) = 40-100 credits/1k.
- Sensitivity and jurisdiction fields are injected by `hive-tenant-gateway`; this crate passes them through.

## What's not yet implemented

- WebSocket streaming for `run_subagent` (currently polls; Phase 2)
- Token-usage-based pricing (currently URN-tier estimate)
- Budget enforcement beyond passing credits_budget through to Honeycomb
