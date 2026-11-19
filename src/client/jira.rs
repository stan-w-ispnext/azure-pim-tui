use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use super::error::PimError;

const MAX_RESULTS: u32 = 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq)]
pub struct JiraIssue {
    pub key: String,
    pub summary: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    issues: Vec<IssueEntry>,
}

#[derive(Deserialize)]
struct IssueEntry {
    key: String,
    #[serde(default)]
    fields: IssueFields,
}

#[derive(Deserialize, Default)]
struct IssueFields {
    #[serde(default)]
    summary: String,
}

pub struct JiraClient {
    client: Client,
    base_url: String,
    email: String,
    api_token: String,
}

impl JiraClient {
    pub fn new(base_url: &str, email: &str, api_token: &str) -> Self {
        // A timeout guarantees every request reports back, so the cache never
        // keeps a key marked in flight forever.
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            email: email.to_string(),
            api_token: api_token.to_string(),
        }
    }

    /// The issue with `key`, or `None` if it doesn't exist or isn't visible.
    /// `key` must already be validated as an issue key.
    pub async fn get_issue(&self, key: &str) -> Result<Option<JiraIssue>> {
        let url = format!("{}/rest/api/3/issue/{key}", self.base_url);
        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.email, Some(&self.api_token))
            .header("Accept", "application/json")
            .query(&[("fields", "summary")])
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(PimError::Api {
                status: status.as_u16(),
                message,
            }
            .into());
        }

        let issue: IssueEntry = resp
            .json()
            .await
            .map_err(|e| PimError::Parse(e.to_string()))?;
        Ok(Some(JiraIssue {
            key: issue.key,
            summary: issue.fields.summary,
        }))
    }

    /// Issues matching `jql`, in the order Jira returns them.
    pub async fn search(&self, jql: &str) -> Result<Vec<JiraIssue>> {
        let url = format!("{}/rest/api/3/search/jql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.email, Some(&self.api_token))
            .header("Accept", "application/json")
            .json(&json!({
                "jql": jql,
                "fields": ["summary"],
                "maxResults": MAX_RESULTS,
            }))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(PimError::Api {
                status: status.as_u16(),
                message,
            }
            .into());
        }

        let body: SearchResponse = resp
            .json()
            .await
            .map_err(|e| PimError::Parse(e.to_string()))?;
        Ok(body
            .issues
            .into_iter()
            .map(|i| JiraIssue {
                key: i.key,
                summary: i.fields.summary,
            })
            .collect())
    }
}
