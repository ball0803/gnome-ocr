//! AnkiConnect HTTP client. AnkiConnect is an Anki plugin that exposes a JSON-RPC over HTTP
//! on 127.0.0.1:8765. We use it to add mining cards directly from the Yomitan popup.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AnkiClient {
    url: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
struct AnkiRequest {
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnkiResponse<T> {
    pub error: Option<String>,
    pub result: Option<T>,
}

impl AnkiClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    pub async fn ping(&self) -> Result<bool> {
        let resp = self.call::<serde_json::Value>("version", None).await?;
        Ok(resp.error.is_none() && resp.result.is_some())
    }

    pub async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        action: &str,
        params: Option<serde_json::Value>,
    ) -> Result<AnkiResponse<T>> {
        let body = AnkiRequest {
            action: action.to_string(),
            version: Some(6),
            params,
        };
        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Anki POST {action}"))?;
        let status = resp.status();
        let parsed = resp.json().await.context("parse Anki response")?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("AnkiConnect HTTP {status}"));
        }
        Ok(parsed)
    }

    /// Add a note. `fields` keys are field names from the model; values are templates
    /// expanded by the caller. `tags`, `deck`, `allowDuplicate` honor the user's config.
    pub async fn add_note(
        &self,
        deck: &str,
        model: &str,
        fields: &HashMap<String, String>,
        tags: &[String],
        allow_duplicate: bool,
    ) -> Result<i64> {
        let note = serde_json::json!({
            "deckName": deck,
            "modelName": model,
            "fields": fields,
            "tags": tags,
            "options": { "allowDuplicate": allow_duplicate },
        });
        let resp: AnkiResponse<i64> = self
            .call("addNote", Some(serde_json::json!({ "note": note })))
            .await?;
        if let Some(err) = resp.error {
            return Err(anyhow::anyhow!("AnkiConnect addNote: {err}"));
        }
        resp.result.context("AnkiConnect addNote returned no result")
    }
}