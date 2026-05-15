use std::time::Duration;
use crate::{GatewayError, GatewayResult};

/// HTTP-backed client for the Honeycomb control plane.
/// Phase 2 will add a NATS-backed alternative behind a trait.
pub struct HoneycombClient {
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl HoneycombClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self { base_url: base_url.into(), api_key, http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    pub(crate) async fn get_json<R: serde::de::DeserializeOwned>(&self, path: &str) -> GatewayResult<R> {
        let mut req = self.http.get(self.url(path));
        if let Some(k) = &self.api_key { req = req.header("x-api-key", k); }
        let resp = req.send().await.map_err(|e| GatewayError::ControlPlane(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(GatewayError::ControlPlane(format!("HTTP {} for GET {}", resp.status(), path)));
        }
        resp.json::<R>().await.map_err(|e| GatewayError::ControlPlane(e.to_string()))
    }

    pub(crate) async fn post_json<B: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> GatewayResult<R> {
        let mut req = self.http.post(self.url(path)).json(body);
        if let Some(k) = &self.api_key { req = req.header("x-api-key", k); }
        let resp = req.send().await.map_err(|e| GatewayError::ControlPlane(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(GatewayError::ControlPlane(format!("HTTP {} for POST {}", resp.status(), path)));
        }
        resp.json::<R>().await.map_err(|e| GatewayError::ControlPlane(e.to_string()))
    }
}
