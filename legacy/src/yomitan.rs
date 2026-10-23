//! HTTP client for yomitan-api (the Native Messaging host that bridges Yomitan's extension
//! to a localhost HTTP API on 127.0.0.1:19633). Provides term lookup, ankiCardFormats, etc.
//!
//! The full Yomitan API spec is at https://github.com/yomidevs/yomitan-api/. We only need
//! term lookup for the popup, plus anki fields if the user wants Anki mining.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct YomitanClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermLookupRequest {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermEntry {
    pub expression: String,
    pub reading: String,
    #[serde(default)]
    pub definitions: Vec<serde_json::Value>,
    #[serde(default)]
    pub glossary: Vec<serde_json::Value>,
    #[serde(default)]
    pub furigana: Vec<serde_json::Value>,
    #[serde(default)]
    pub pitches: Vec<serde_json::Value>,
    #[serde(default)]
    #[allow(non_snake_case)]
    pub termTags: Vec<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sentence: serde_json::Value,
    #[serde(default)]
    pub has_structural_data: bool,
    #[serde(default)]
    pub audio: serde_json::Value,
    #[serde(default)]
    pub frequencies: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LookupReply {
    Multi(Vec<Vec<TermEntry>>),
    Single(Vec<TermEntry>),
}

impl YomitanClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    pub async fn server_version(&self) -> Result<String> {
        let resp = self
            .http
            .get(format!("{}/serverVersion", self.base_url))
            .send()
            .await
            .context("yomitan-api /serverVersion")?;
        resp.error_for_status()?.text().await.context("yomitan-api body")
    }

    pub async fn lookup(&self, text: &str) -> Result<LookupReply> {
        let resp = self
            .http
            .post(format!("{}/lookup", self.base_url))
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .context("yomitan-api /lookup POST")?;
        let status = resp.status();
        let body = resp.text().await.context("yomitan-api body")?;
        if !status.is_success() {
            // Common case: extension not running, API not reachable.
            return Err(anyhow::anyhow!("yomitan-api HTTP {status}: {body}"));
        }
        let reply: LookupReply = serde_json::from_str(&body).context("parse /lookup reply")?;
        Ok(reply)
    }

    pub async fn anki_fields(&self, text: &str, terms: &[String]) -> Result<serde_json::Value> {
        let mut query: Vec<(&str, String)> = vec![("text", text.to_string())];
        for t in terms {
            query.push(("term", t.clone()));
        }
        let resp = self
            .http
            .get(format!("{}/ankiFields", self.base_url))
            .query(&query)
            .send()
            .await
            .context("yomitan-api /ankiFields")?;
        resp.error_for_status()?.json().await.context("parse /ankiFields")
    }
}