use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_client_id")]
    pub default_client_id: String,
    #[serde(default = "default_justification")]
    pub default_justification: String,
    #[serde(default = "default_duration_hours")]
    pub default_duration_hours: u32,
    #[serde(default = "default_auto_refresh_secs")]
    pub auto_refresh_secs: u64,
    /// Show ticket number/system fields in the activation modal, for tenants
    /// whose PIM policy requires ticket information on activation.
    #[serde(default)]
    pub ticket_fields_enabled: bool,
    #[serde(default)]
    pub default_ticket_system: String,
    /// Jira site, e.g. `https://your-org.atlassian.net`. When set together with
    /// `jira_email` and an API token, the activation modal prefills the ticket
    /// number with your in-progress issue.
    #[serde(default)]
    pub jira_url: String,
    #[serde(default)]
    pub jira_email: String,
    /// Falls back to the `JIRA_API_TOKEN` environment variable when empty.
    #[serde(default)]
    pub jira_api_token: String,
    #[serde(default = "default_jira_jql")]
    pub jira_jql: String,
}

/// Microsoft Graph Command Line Tools app ID — first-party Microsoft app with
/// broad Graph permissions pre-consented, including PIM for Groups.
fn default_client_id() -> String {
    "14d82eec-204b-4c2f-b7e8-296a70dab67e".to_string()
}

fn default_justification() -> String {
    "Local development".to_string()
}

fn default_duration_hours() -> u32 {
    8
}

fn default_auto_refresh_secs() -> u64 {
    60
}

fn default_jira_jql() -> String {
    "assignee = currentUser() AND status = 'In Progress' ORDER BY updated DESC".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_client_id: default_client_id(),
            default_justification: default_justification(),
            default_duration_hours: default_duration_hours(),
            auto_refresh_secs: default_auto_refresh_secs(),
            ticket_fields_enabled: false,
            default_ticket_system: String::new(),
            jira_url: String::new(),
            jira_email: String::new(),
            jira_api_token: String::new(),
            jira_jql: default_jira_jql(),
        }
    }
}

impl Config {
    /// Jira `(url, email, api_token)` when the integration is fully configured.
    pub fn jira_credentials(&self) -> Option<(String, String, String)> {
        let token = if self.jira_api_token.is_empty() {
            std::env::var("JIRA_API_TOKEN").unwrap_or_default()
        } else {
            self.jira_api_token.clone()
        };
        (!self.jira_url.is_empty() && !self.jira_email.is_empty() && !token.is_empty())
            .then(|| (self.jira_url.clone(), self.jira_email.clone(), token))
    }

    /// `~/.config/azure-pim-tui` when it exists (the documented location, also
    /// honoured on macOS), otherwise the platform config dir.
    fn config_dir() -> PathBuf {
        if let Some(xdg) = dirs::home_dir().map(|h| h.join(".config").join("azure-pim-tui")) {
            if xdg.is_dir() {
                return xdg;
            }
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("azure-pim-tui")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn log_path() -> PathBuf {
        Self::config_dir().join("azure-pim-tui.log")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let config: Config = toml::from_str(&content)?;
            Ok(config)
        } else {
            let config = Config::default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let content = toml::to_string_pretty(&config)?;
            std::fs::write(&path, content)?;
            Ok(config)
        }
    }
}
