use std::time::Duration;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{GatewayError, GatewayResult, HoneycombClient};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityInfo {
    pub urn: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub avg_cost_credits: u64,
    pub workers_available: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeClusterResponse {
    pub capabilities: Vec<CapabilityInfo>,
    pub nodes_online: u32,
    pub avg_latency_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSubagentRequest {
    pub capability_urn: String,
    pub input: serde_json::Value,
    /// Hard deadline. Defaults to 60s if None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSubagentResponse {
    pub task_id: Uuid,
    pub status: String,           // "completed" | "failed" | "timeout"
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub credits_spent: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateCostRequest {
    pub capability_urn: String,
    pub input_size_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimateCostResponse {
    pub credits_estimate: u64,
    pub multipliers_applied: serde_json::Value, // raw shape from Honeycomb
}

#[async_trait::async_trait]
pub trait McpTools: Send + Sync {
    async fn describe_cluster(&self) -> GatewayResult<DescribeClusterResponse>;
    async fn run_subagent(&self, req: RunSubagentRequest) -> GatewayResult<RunSubagentResponse>;
    async fn estimate_cost(&self, req: EstimateCostRequest) -> GatewayResult<EstimateCostResponse>;
}

/// HTTP-backed MCP tools. Calls into Honeycomb's REST API.
pub struct HttpMcpTools { pub client: HoneycombClient }

impl HttpMcpTools {
    pub fn new(client: HoneycombClient) -> Self { Self { client } }
}

#[async_trait::async_trait]
impl McpTools for HttpMcpTools {
    async fn describe_cluster(&self) -> GatewayResult<DescribeClusterResponse> {
        // Endpoint lands in Batch 2.2; until then we report Unsupported.
        // Once /api/capabilities exists this becomes:
        //   self.client.get_json("/api/capabilities").await
        Err(GatewayError::Unsupported("describe_cluster requires Honeycomb /api/capabilities (Batch 2.2)"))
    }

    async fn run_subagent(&self, req: RunSubagentRequest) -> GatewayResult<RunSubagentResponse> {
        // Submit task via Honeycomb's existing /api/tasks/create endpoint.
        // The capability_urn -> execution_type translation happens here in v1
        // until Batch 2.2 lands URN-native routing.
        let body = serde_json::json!({
            "task_id": Uuid::new_v4(),
            "capability_urn": req.capability_urn,
            "payload": req.input,
        });
        #[derive(Deserialize)]
        struct TaskCreated { task_id: Uuid }
        let created: TaskCreated = self.client.post_json("/api/tasks/create", &body).await?;

        let timeout = Duration::from_secs(req.timeout_seconds.unwrap_or(60));
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(GatewayError::TaskTimeout { seconds: timeout.as_secs() });
            }

            #[derive(Deserialize)]
            struct TaskView {
                status: String,
                output: Option<serde_json::Value>,
                last_error: Option<String>,
                credits_spent: Option<u64>,
            }
            let view: TaskView = self.client
                .get_json(&format!("/api/tasks/{}", created.task_id))
                .await?;

            if view.status == "completed" || view.status == "failed" {
                return Ok(RunSubagentResponse {
                    task_id: created.task_id,
                    status: view.status,
                    output: view.output,
                    error: view.last_error,
                    credits_spent: view.credits_spent.unwrap_or(0),
                });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn estimate_cost(&self, _req: EstimateCostRequest) -> GatewayResult<EstimateCostResponse> {
        Err(GatewayError::Unsupported("estimate_cost requires Honeycomb /api/costs/estimate (Batch 2.2)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_subagent_request_roundtrips_json() {
        let req = RunSubagentRequest {
            capability_urn: "oasf://commons/capability/classify/v2".into(),
            input: serde_json::json!({"text": "hi"}),
            timeout_seconds: Some(30),
        };
        let s = serde_json::to_string(&req).unwrap();
        let r2: RunSubagentRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(r2.capability_urn, req.capability_urn);
        assert_eq!(r2.timeout_seconds, Some(30));
    }

    #[tokio::test]
    async fn describe_cluster_returns_unsupported_until_endpoint_lands() {
        let tools = HttpMcpTools::new(HoneycombClient::new("http://localhost:1", None));
        match tools.describe_cluster().await {
            Err(GatewayError::Unsupported(msg)) => assert!(msg.contains("describe_cluster")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn estimate_cost_returns_unsupported_until_endpoint_lands() {
        let tools = HttpMcpTools::new(HoneycombClient::new("http://localhost:1", None));
        let req = EstimateCostRequest { capability_urn: "oasf://x/y/z/v1".into(), input_size_tokens: 1000 };
        match tools.estimate_cost(req).await {
            Err(GatewayError::Unsupported(msg)) => assert!(msg.contains("estimate_cost")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
