use std::time::Duration;
use chrono::Utc;
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
    /// Sensitivity tier required for this task. Injected by the Tenant Gateway
    /// from the tenant's plan; overridden by the gateway — not accepted from
    /// the raw caller body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity_required: Option<String>,
    /// Jurisdiction tags the executing comb must satisfy. Injected by the
    /// Tenant Gateway from the tenant's data-residency settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jurisdiction_required: Vec<String>,
    /// Credits budget to embed in the ACP envelope's BudgetContext. Set by
    /// the Tenant Gateway from the ledger balance after the pre-call debit;
    /// defaults to a conservative fallback when the ledger is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_budget: Option<i64>,
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
        let task_id = Uuid::new_v4();
        let body = build_task_create_body(
            task_id,
            &urn,
            &profile,
            &req.prompt,
            req.tenant_id,
            req.sensitivity_required.clone(),
            req.jurisdiction_required.clone(),
            req.credits_budget,
        )?;
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

    async fn estimate_cost(&self, req: EstimateCostRequest) -> GatewayResult<EstimateCostResponse> {
        // Phase 1 estimate: credits-per-1k-tokens derived from capability URN.
        // Business model §2: 1 credit = 1 second on a mid-tier 7B CPU comb.
        // Estimated throughput: 7B CPU = 50 tok/s → 20 credits/1k tokens;
        //                       70B CPU = 10 tok/s → 100 credits/1k tokens;
        //                       7B GPU = 200 tok/s → 5 credits/1k tokens.
        // Flat-rate capabilities (code/exec, web/search) return a job-level estimate.
        let urn = req.capability_urn.to_lowercase();
        let (credits_per_1k, category) = if urn.contains("70b") {
            (100u64, "inference/70b-cpu")
        } else if urn.contains("13b") {
            (30, "inference/13b-cpu")
        } else if urn.contains("gpu") {
            (5, "inference/7b-gpu")
        } else if urn.contains("/code/") || urn.contains("/exec/") {
            (2000, "code/exec") // ~2 credits/job; use 1k-token base → 2 credits/call
        } else if urn.contains("/web/") || urn.contains("/search/") {
            (5000, "web/search") // 5 credits/call
        } else {
            (20, "inference/7b-cpu")
        };

        let credits = ((req.input_size_tokens.saturating_mul(credits_per_1k)) + 999) / 1000;
        let credits = credits.max(1);

        Ok(EstimateCostResponse {
            credits_estimate: credits,
            multipliers_applied: serde_json::json!({
                "capability_urn": req.capability_urn,
                "category": category,
                "credits_per_1k_tokens": credits_per_1k,
                "input_size_tokens": req.input_size_tokens,
                "note": "Phase 1 estimate — based on model size from URN, not profiled benchmarks"
            }),
        })
    }
}

/// Build an `AcpEnvelope<TaskCreateRequest>` body ready to POST to Honeycomb.
///
/// The envelope carries W3C traceparent (freshly minted), an idempotency key
/// (task_id hex), and a conservative BudgetContext. Honeycomb's ingest path
/// accepts both bare `TaskCreateRequest` (back-compat) and this envelope form;
/// the envelope form is preferred because it propagates traceparent and budget
/// constraints end-to-end.
///
/// Pulled out from `run_subagent` so a unit test can assert the body
/// round-trips through `hive_sdk::AcpEnvelope<TaskCreateRequest>` — that's the
/// contract with Honeycomb's deserialiser.
fn build_task_create_body(
    task_id: Uuid,
    urn: &str,
    profile: &str,
    prompt: &str,
    tenant_id: Option<Uuid>,
    sensitivity_required: Option<String>,
    jurisdiction_required: Vec<String>,
    credits_budget: Option<i64>,
) -> GatewayResult<serde_json::Value> {
    let capability_urn = parse_urn(urn)?;
    let mut inner = serde_json::json!({
        "task_id": task_id,
        "owner_id": Uuid::nil(),
        "execution_type": "llm",
        "payload": {
            "profile": profile,
            "prompt": prompt,
        },
        "required_capabilities": {
            "cpu_cores": null,
            "memory_mb": null,
            "llm_profiles": []
        },
        "allowed_nodes": "hive-wide",
        "capability_urn": capability_urn,
    });
    if let Some(tid) = tenant_id {
        inner["tenant_id"] = serde_json::Value::String(tid.to_string());
    }
    if let Some(sens) = sensitivity_required {
        inner["sensitivity_required"] = serde_json::Value::String(sens);
    }
    if !jurisdiction_required.is_empty() {
        inner["jurisdiction_required"] = serde_json::to_value(jurisdiction_required)
            .unwrap_or(serde_json::Value::Array(vec![]));
    }

    let traceparent = mint_traceparent(task_id);
    let origin_user_id = tenant_id.unwrap_or_else(Uuid::nil);
    let deadline = Utc::now() + chrono::Duration::seconds(60);
    // Use caller-supplied balance (post-debit) when available; fall back to a
    // conservative default so dev/MCP-stdio paths still get a valid envelope.
    let credits = credits_budget.unwrap_or(1_000);
    let envelope = serde_json::json!({
        "task_id": task_id,
        "traceparent": traceparent,
        "idempotency_key": task_id.simple().to_string(),
        "budget_context": {
            "input_tokens_remaining": 4096_i64,
            "output_tokens_remaining": 4096_i64,
            "credits_remaining": credits,
            "spawn_depth_remaining": 4_u32,
            "wall_clock_deadline": deadline.to_rfc3339(),
        },
        "origin_user_id": origin_user_id,
        "payload": inner,
    });
    Ok(envelope)
}

/// Mint a W3C traceparent for a known task_id.
/// Format: `00-{trace_id_32hex}-{span_id_16hex}-01`
fn mint_traceparent(task_id: Uuid) -> String {
    let trace_id = task_id.simple().to_string();
    let span = Uuid::new_v4().simple().to_string();
    let span_id = &span[0..16];
    format!("00-{trace_id}-{span_id}-01")
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
            sensitivity_required: None,
            jurisdiction_required: Vec::new(),
            credits_budget: None,
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
            sensitivity_required: None,
            jurisdiction_required: Vec::new(),
            credits_budget: None,
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
            sensitivity_required: None,
            jurisdiction_required: Vec::new(),
            credits_budget: None,
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
            sensitivity_required: None,
            jurisdiction_required: Vec::new(),
            credits_budget: None,
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

    #[test]
    fn task_create_body_is_valid_acp_envelope_for_honeycomb() {
        // The shape we POST to /api/tasks/create must deserialise as an
        // `AcpEnvelope<TaskCreateRequest>`. Honeycomb tries the envelope form
        // first; if this test breaks every gateway call silently degrades to the
        // back-compat bare-request path and loses traceparent + budget context.
        let task_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let body = build_task_create_body(
            task_id,
            "oasf://commons/inference/qwen2.5-0.5b/v1",
            "default",
            "Classify: 'great game!'",
            Some(tenant_id),
            None,
            Vec::new(),
            Some(500),
        )
        .unwrap();

        // Must round-trip through AcpEnvelope<TaskCreateRequest>.
        let envelope: hive_sdk::AcpEnvelope<hive_sdk::TaskCreateRequest> =
            serde_json::from_value(body.clone())
                .expect("body deserialises as AcpEnvelope<TaskCreateRequest>");
        assert_eq!(envelope.task_id, task_id);
        assert!(envelope.traceparent.starts_with("00-"), "traceparent in W3C format");

        // Inner payload must be a valid TaskCreateRequest.
        let inner = envelope.payload;
        assert_eq!(inner.task_id, task_id);
        assert_eq!(inner.tenant_id, Some(tenant_id));
        assert_eq!(inner.allowed_nodes, hive_sdk::AllowedNodesScope::HiveWide);
        let urn = inner.capability_urn.expect("capability_urn present");
        assert_eq!(urn.namespace, "commons");
        assert_eq!(urn.operation, "qwen2.5-0.5b");
        assert_eq!(urn.version, 1);

        // Budget context must have sane defaults.
        assert!(envelope.budget_context.credits_remaining > 0);
        assert!(envelope.budget_context.spawn_depth_remaining > 0);
        assert!(envelope.budget_context.wall_clock_deadline > chrono::Utc::now());
    }

    #[test]
    fn task_create_body_inner_omits_tenant_id_when_none() {
        let body = build_task_create_body(
            Uuid::new_v4(),
            "oasf://commons/inference/qwen2.5-0.5b/v1",
            "default",
            "x",
            None,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        // tenant_id absent in the inner payload.
        assert!(body["payload"].get("tenant_id").is_none());
    }

    #[test]
    fn task_create_body_threads_sensitivity_and_jurisdiction() {
        let body = build_task_create_body(
            Uuid::new_v4(),
            "oasf://commons/inference/qwen2.5-0.5b/v1",
            "default",
            "Classify: 'great game!'",
            None,
            Some("semi_private".to_string()),
            vec!["eu-gdpr".to_string()],
            None,
        )
        .unwrap();
        let envelope: hive_sdk::AcpEnvelope<hive_sdk::TaskCreateRequest> =
            serde_json::from_value(body).unwrap();
        let inner = envelope.payload;
        assert_eq!(
            inner.sensitivity_required,
            Some(hive_sdk::Sensitivity::SemiPrivate)
        );
        assert_eq!(inner.jurisdiction_required, vec!["eu-gdpr".to_string()]);
    }

    #[test]
    fn task_create_body_credits_budget_overrides_default() {
        let body = build_task_create_body(
            Uuid::new_v4(),
            "oasf://commons/inference/qwen2.5-0.5b/v1",
            "default",
            "x",
            None,
            None,
            Vec::new(),
            Some(42),
        )
        .unwrap();
        assert_eq!(body["budget_context"]["credits_remaining"], 42);
    }

    #[test]
    fn task_create_body_credits_budget_defaults_to_1000_when_none() {
        let body = build_task_create_body(
            Uuid::new_v4(),
            "oasf://commons/inference/qwen2.5-0.5b/v1",
            "default",
            "x",
            None,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(body["budget_context"]["credits_remaining"], 1000);
    }

    #[tokio::test]
    async fn estimate_cost_returns_credits_based_on_urn() {
        let tools = HttpMcpTools::new(HoneycombClient::new("http://localhost:1", None));
        // 7B CPU model: 20 credits/1k tokens → 1000 tokens = 20 credits
        let resp = tools
            .estimate_cost(EstimateCostRequest {
                capability_urn: "oasf://commons/inference/qwen2.5-0.5b/v1".into(),
                input_size_tokens: 1000,
            })
            .await
            .expect("estimate should succeed");
        assert_eq!(resp.credits_estimate, 20);

        // 70B model: 100 credits/1k tokens → 500 tokens = 50 credits
        let resp70b = tools
            .estimate_cost(EstimateCostRequest {
                capability_urn: "oasf://commons/inference/llama3-70b/v1".into(),
                input_size_tokens: 500,
            })
            .await
            .expect("estimate should succeed");
        assert_eq!(resp70b.credits_estimate, 50);

        // Minimum 1 credit even for tiny inputs
        let resp_min = tools
            .estimate_cost(EstimateCostRequest {
                capability_urn: "oasf://commons/inference/qwen2.5-0.5b/v1".into(),
                input_size_tokens: 1,
            })
            .await
            .expect("estimate should succeed");
        assert_eq!(resp_min.credits_estimate, 1);
    }
}
