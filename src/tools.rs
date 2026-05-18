use std::time::Duration;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{GatewayError, GatewayResult, HoneycombClient};

/// One capability entry as returned by Honeycomb's `/api/capabilities`.
/// Matches the `CapabilityEntry` schema from `hive-sdk::capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityInfo {
    pub urn: String,
    pub workload: String,
    pub description: String,
    pub model_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lora_ref: Option<String>,
    pub latency_p50_ms: u32,
    pub min_tier_band: String,
    pub lifecycle: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeClusterResponse {
    pub capabilities: Vec<CapabilityInfo>,
}

/// The MCP `run_subagent` tool. Generic-inference shape:
///
/// - The caller provides a **prompt** (the workload — classify, summarise,
///   extract, whatever — lives entirely in the prompt text).
/// - The caller picks a **model** by either `model_id` (canonical Ollama-style
///   tag, e.g. `"qwen2.5:0.5b"`) or `capability_urn` (the full URN form,
///   e.g. `"oasf://commons/inference/qwen2.5-0.5b/v1"`). Exactly one is
///   required; if both are given, `capability_urn` wins.
/// - Optional `profile` names the comb-side LLM profile (defaults to
///   `"default"`); optional `timeout_seconds` (defaults to 60).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSubagentRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_urn: Option<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Tenant the call belongs to. Set by the Tenant Gateway from the
    /// authenticated bearer; never trusted from a raw client. The MCP-stdio
    /// path leaves this `None` (single-user dev). Honeycomb stamps it onto
    /// the `TaskRecord` for audit/scoping. See
    /// `docs/02_architecture/18_tenant_gateway.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
}

impl RunSubagentRequest {
    /// Resolve the canonical capability URN from `capability_urn` (preferred) or
    /// `model_id` (derived). Returns an error if neither is set.
    pub fn resolved_capability_urn(&self) -> GatewayResult<String> {
        if let Some(urn) = &self.capability_urn {
            return Ok(urn.clone());
        }
        if let Some(id) = &self.model_id {
            let id = id.trim();
            if id.is_empty() {
                return Err(GatewayError::Invalid("model_id is empty".into()));
            }
            return Ok(format!(
                "oasf://commons/inference/{}/v1",
                id.replace(':', "-")
            ));
        }
        Err(GatewayError::Invalid(
            "run_subagent requires `capability_urn` or `model_id`".into(),
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSubagentResponse {
    pub task_id: Uuid,
    pub status: String,           // "completed" | "failed" | "timeout"
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
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
        #[derive(Deserialize)]
        struct CapabilitiesResponse {
            capabilities: Vec<CapabilityInfo>,
        }
        let resp: CapabilitiesResponse = self.client.get_json("/api/capabilities").await?;
        Ok(DescribeClusterResponse {
            capabilities: resp.capabilities,
        })
    }

    async fn run_subagent(&self, req: RunSubagentRequest) -> GatewayResult<RunSubagentResponse> {
        let urn = req.resolved_capability_urn()?;
        let profile = req.profile.clone().unwrap_or_else(|| "default".to_string());
        if req.prompt.trim().is_empty() {
            return Err(GatewayError::Invalid("prompt is empty".into()));
        }

        // Generic-inference shape: ExecutionType::Llm + LlmTaskPayload{profile, prompt}.
        // The capability_urn carries the model identity for the scheduler's hard filter;
        // the prompt carries the workload (classify, extract, summarise, …).
        let task_id = Uuid::new_v4();
        let mut body = serde_json::json!({
            "task_id": task_id,
            "owner_id": Uuid::nil(),
            "execution_type": "llm",
            "payload": {
                "profile": profile,
                "prompt": req.prompt,
            },
            "required_capabilities": {
                "cpu_cores": null,
                "memory_mb": null,
                "llm_profiles": []
            },
            "allowed_nodes": "hive_wide",
            "capability_urn": parse_urn(&urn)?,
        });
        if let Some(tid) = req.tenant_id {
            body["tenant_id"] = serde_json::Value::String(tid.to_string());
        }
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
            }
            let view: TaskView = self.client
                .get_json(&format!("/api/tasks/{}", created.task_id))
                .await?;

            if view.status.eq_ignore_ascii_case("succeeded")
                || view.status.eq_ignore_ascii_case("completed")
                || view.status.eq_ignore_ascii_case("failed")
                || view.status.eq_ignore_ascii_case("timed_out")
                || view.status.eq_ignore_ascii_case("cancelled")
            {
                return Ok(RunSubagentResponse {
                    task_id: created.task_id,
                    status: view.status,
                    output: view.output,
                    error: view.last_error,
                });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn estimate_cost(&self, _req: EstimateCostRequest) -> GatewayResult<EstimateCostResponse> {
        Err(GatewayError::Unsupported(
            "estimate_cost requires the Honey Ledger (Phase 2)",
        ))
    }
}

/// Parse a canonical `oasf://<ns>/<domain>/<op>/v<N>` URN into the structured
/// shape Honeycomb's `TaskCreateRequest.capability_urn` field expects.
fn parse_urn(urn: &str) -> GatewayResult<serde_json::Value> {
    // hive_sdk's CapabilityUrn::parse would do this, but we don't depend on hive_sdk
    // directly here; do it inline with the same rules.
    let stripped = urn
        .strip_prefix("oasf://")
        .ok_or_else(|| GatewayError::Invalid(format!("URN must start with oasf://: {urn}")))?;
    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() != 4 {
        return Err(GatewayError::Invalid(format!(
            "URN must have 4 segments after oasf://: {urn}"
        )));
    }
    let version_str = parts[3]
        .strip_prefix('v')
        .ok_or_else(|| GatewayError::Invalid(format!("URN version must start with 'v': {urn}")))?;
    let version: u32 = version_str
        .parse()
        .map_err(|_| GatewayError::Invalid(format!("URN version not an integer: {urn}")))?;
    Ok(serde_json::json!({
        "namespace": parts[0],
        "domain": parts[1],
        "operation": parts[2],
        "version": version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_subagent_request_roundtrips_json_with_model_id() {
        let req = RunSubagentRequest {
            model_id: Some("qwen2.5:0.5b".into()),
            capability_urn: None,
            prompt: "Classify: 'great game!' as positive or negative.".into(),
            profile: Some("default".into()),
            timeout_seconds: Some(30),
            tenant_id: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        let r2: RunSubagentRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(r2.model_id, Some("qwen2.5:0.5b".into()));
        assert_eq!(r2.prompt, req.prompt);
        assert_eq!(r2.timeout_seconds, Some(30));
    }

    #[test]
    fn resolved_capability_urn_prefers_explicit_urn_over_model_id() {
        let req = RunSubagentRequest {
            model_id: Some("qwen2.5:0.5b".into()),
            capability_urn: Some("oasf://commons/inference/gemma-2b/v1".into()),
            prompt: "x".into(),
            profile: None,
            timeout_seconds: None,
            tenant_id: None,
        };
        assert_eq!(
            req.resolved_capability_urn().unwrap(),
            "oasf://commons/inference/gemma-2b/v1"
        );
    }

    #[test]
    fn resolved_capability_urn_derives_from_model_id() {
        let req = RunSubagentRequest {
            model_id: Some("qwen2.5:0.5b".into()),
            capability_urn: None,
            prompt: "x".into(),
            profile: None,
            timeout_seconds: None,
            tenant_id: None,
        };
        assert_eq!(
            req.resolved_capability_urn().unwrap(),
            "oasf://commons/inference/qwen2.5-0.5b/v1"
        );
    }

    #[test]
    fn resolved_capability_urn_errors_when_neither_provided() {
        let req = RunSubagentRequest {
            model_id: None,
            capability_urn: None,
            prompt: "x".into(),
            profile: None,
            timeout_seconds: None,
            tenant_id: None,
        };
        assert!(matches!(
            req.resolved_capability_urn(),
            Err(GatewayError::Invalid(_))
        ));
    }

    #[test]
    fn parse_urn_round_trips_canonical() {
        let v = parse_urn("oasf://commons/inference/qwen2.5-0.5b/v1").unwrap();
        assert_eq!(v["namespace"], "commons");
        assert_eq!(v["domain"], "inference");
        assert_eq!(v["operation"], "qwen2.5-0.5b");
        assert_eq!(v["version"], 1);
    }

    #[test]
    fn parse_urn_rejects_wrong_scheme() {
        assert!(parse_urn("https://commons/inference/q/v1").is_err());
    }

    #[test]
    fn parse_urn_rejects_missing_version_prefix() {
        assert!(parse_urn("oasf://commons/inference/q/2").is_err());
    }

    #[tokio::test]
    async fn estimate_cost_returns_unsupported_until_ledger_lands() {
        let tools = HttpMcpTools::new(HoneycombClient::new("http://localhost:1", None));
        let req = EstimateCostRequest {
            capability_urn: "oasf://commons/inference/qwen2.5-0.5b/v1".into(),
            input_size_tokens: 1000,
        };
        match tools.estimate_cost(req).await {
            Err(GatewayError::Unsupported(msg)) => assert!(msg.contains("Honey Ledger")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
