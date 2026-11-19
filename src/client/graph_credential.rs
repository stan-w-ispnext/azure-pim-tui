use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::error::PimError;

const GRAPH_SCOPES: &str = "https://graph.microsoft.com/PrivilegedAccess.ReadWrite.AzureADGroup https://graph.microsoft.com/PrivilegedEligibilitySchedule.Read.AzureADGroup https://graph.microsoft.com/GroupMember.Read.All offline_access";

/// How long to wait for the user to finish signing in in the browser.
const BROWSER_TIMEOUT_SECS: u64 = 300;

/// User agent that identifies the real OS so Conditional Access policies that
/// block "unknown" device platforms can classify the request. It reports the
/// platform this binary is actually running on.
#[cfg(target_os = "macos")]
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) azure-pim-tui";
#[cfg(target_os = "windows")]
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) azure-pim-tui";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) azure-pim-tui";

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    #[allow(dead_code)]
    user_code: String,
    #[allow(dead_code)]
    verification_uri: String,
    expires_in: u64,
    interval: u64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: u64, // unix timestamp
}

pub struct GraphCredential {
    client: reqwest::Client,
    tenant_id: String,
    client_id: String,
    cached: Mutex<Option<CachedToken>>,
    status_tx: Option<mpsc::UnboundedSender<String>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn random_b64(num_bytes: usize) -> String {
    // rand::random works across rand versions (0.8, 0.9, ...)
    let buf: Vec<u8> = (0..num_bytes).map(|_| rand::random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(buf)
}

/// Best-effort attempt to open the default browser. Returns true on success.
fn open_browser(url: &str) -> bool {
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let result = Command::new("open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(target_os = "windows")]
    let result = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result = Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    result.map(|s| s.success()).unwrap_or(false)
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

impl GraphCredential {
    pub fn new(
        tenant_id: String,
        client_id: String,
        status_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> Self {
        let cached = Self::load_cached_token();
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            tenant_id,
            client_id,
            cached: Mutex::new(cached),
            status_tx,
        }
    }

    fn cache_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("azure-pim-tui")
            .join("graph_token.json")
    }

    fn load_cached_token() -> Option<CachedToken> {
        let path = Self::cache_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path).ok()?;
            let token: CachedToken = serde_json::from_str(&content).ok()?;
            debug!("Loaded cached Graph token from {}", path.display());
            Some(token)
        } else {
            None
        }
    }

    fn save_cached_token(token: &CachedToken) {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(token) {
            // The file holds access and refresh tokens, so keep it private.
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&path)
                {
                    let _ = f.write_all(content.as_bytes());
                }
            }
            #[cfg(not(unix))]
            {
                let _ = std::fs::write(&path, content);
            }
            debug!("Saved Graph token cache to {}", path.display());
        }
    }

    fn send_status(&self, msg: String) {
        if let Some(tx) = &self.status_tx {
            let _ = tx.send(msg);
        }
    }

    fn token_url(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        )
    }

    pub async fn get_token(&self) -> Result<String> {
        let mut cached = self.cached.lock().await;

        // Check if we have a valid cached token
        if let Some(ref token) = *cached {
            let now = now_secs();

            // Token still valid (with 5 min buffer)
            if now + 300 < token.expires_at {
                debug!(
                    "Using cached Graph token (expires in {}s)",
                    token.expires_at - now
                );
                return Ok(token.access_token.clone());
            }

            // Try refresh
            if let Some(ref refresh_token) = token.refresh_token {
                debug!("Attempting token refresh");
                match self.refresh_token(refresh_token).await {
                    Ok(new_token) => {
                        let result = new_token.access_token.clone();
                        *cached = Some(new_token);
                        Self::save_cached_token(cached.as_ref().unwrap());
                        return Ok(result);
                    }
                    Err(e) => {
                        warn!("Token refresh failed: {e}, signing in again");
                    }
                }
            }
        }

        // Need fresh auth. Prefer the browser flow; fall back to device code
        // (headless/SSH sessions). Set PIM_AUTH=device to force device code.
        let force_device = std::env::var("PIM_AUTH")
            .map(|v| v.eq_ignore_ascii_case("device"))
            .unwrap_or(false);

        let new_token = if force_device {
            self.device_code_flow().await?
        } else {
            match self.browser_flow().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("Browser sign-in failed: {e:#}. Falling back to device code flow");
                    self.device_code_flow().await?
                }
            }
        };

        let result = new_token.access_token.clone();
        *cached = Some(new_token);
        Self::save_cached_token(cached.as_ref().unwrap());
        Ok(result)
    }

    /// Turn a token endpoint response into a CachedToken.
    async fn parse_token_response(
        resp: reqwest::Response,
        what: &str,
        prior_refresh: Option<&str>,
    ) -> Result<CachedToken> {
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(PimError::Auth(format!("{what} failed: {body}")).into());
        }
        let token_resp: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse token response")?;
        Ok(CachedToken {
            access_token: token_resp.access_token,
            refresh_token: token_resp
                .refresh_token
                .or_else(|| prior_refresh.map(|s| s.to_string())),
            expires_at: now_secs() + token_resp.expires_in,
        })
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<CachedToken> {
        let resp = self
            .client
            .post(self.token_url())
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("scope", GRAPH_SCOPES),
            ])
            .send()
            .await
            .context("Failed to refresh token")?;

        Self::parse_token_response(resp, "Token refresh", Some(refresh_token)).await
    }

    /// Interactive sign-in: authorization code flow with PKCE and a loopback
    /// redirect. The sign-in happens in the user's normal browser, so the
    /// platform is detected the same way as for Connect-MgGraph.
    async fn browser_flow(&self) -> Result<CachedToken> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to start local callback listener")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://localhost:{port}");

        // PKCE + CSRF state
        let verifier = random_b64(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_b64(16);

        let auth_url = Url::parse_with_params(
            &format!(
                "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
                self.tenant_id
            ),
            &[
                ("client_id", self.client_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", redirect_uri.as_str()),
                ("response_mode", "query"),
                ("scope", GRAPH_SCOPES),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", state.as_str()),
            ],
        )
        .context("Failed to build authorization URL")?;

        info!("Starting browser sign-in for Graph API permissions");
        if open_browser(auth_url.as_str()) {
            self.send_status("Complete the sign-in in your browser...".to_string());
        } else {
            self.send_status(format!(
                "Open this URL in your browser to sign in:\n{auth_url}"
            ));
        }

        // Wait for the redirect back to localhost with ?code=...
        let params: HashMap<String, String> = loop {
            let (mut stream, _) =
                tokio::time::timeout(Duration::from_secs(BROWSER_TIMEOUT_SECS), listener.accept())
                    .await
                    .map_err(|_| PimError::Auth("Browser sign-in timed out".to_string()))??;

            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();

            let parsed = Url::parse(&format!("http://localhost{path}"))
                .map(|u| u.query_pairs().into_owned().collect::<HashMap<_, _>>())
                .unwrap_or_default();

            if parsed.contains_key("code") || parsed.contains_key("error") {
                respond(
                    &mut stream,
                    "200 OK",
                    "<html><body style=\"font-family:sans-serif\"><h3>Signed in.</h3>\
                     <p>You can close this tab and return to the terminal.</p></body></html>",
                )
                .await;
                break parsed;
            }

            // Stray request (favicon etc.), ignore and keep waiting.
            respond(&mut stream, "404 Not Found", "").await;
        };

        if let Some(err) = params.get("error") {
            let desc = params.get("error_description").cloned().unwrap_or_default();
            return Err(PimError::Auth(format!("Browser sign-in failed: {err} - {desc}")).into());
        }
        if params.get("state").map(String::as_str) != Some(state.as_str()) {
            return Err(PimError::Auth("Sign-in state mismatch, aborting".to_string()).into());
        }
        let code = params
            .get("code")
            .ok_or_else(|| PimError::Auth("No authorization code returned".to_string()))?;

        let resp = self
            .client
            .post(self.token_url())
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("code_verifier", verifier.as_str()),
                ("scope", GRAPH_SCOPES),
            ])
            .send()
            .await
            .context("Failed to exchange authorization code")?;

        let token = Self::parse_token_response(resp, "Authorization code exchange", None).await?;
        self.send_status("Graph API authenticated".to_string());
        Ok(token)
    }

    async fn device_code_flow(&self) -> Result<CachedToken> {
        let device_code_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
            self.tenant_id
        );

        info!("Starting device code flow for Graph API permissions");

        let resp = self
            .client
            .post(&device_code_url)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("scope", GRAPH_SCOPES),
            ])
            .send()
            .await
            .context("Failed to initiate device code flow")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(PimError::Auth(format!("Device code request failed: {body}")).into());
        }

        let dc_resp: DeviceCodeResponse = resp
            .json()
            .await
            .context("Failed to parse device code response")?;

        // Notify the TUI to display the code
        self.send_status(dc_resp.message.clone());
        info!("{}", dc_resp.message);

        let token_url = self.token_url();
        let interval = Duration::from_secs(dc_resp.interval.max(5));
        let deadline = std::time::Instant::now() + Duration::from_secs(dc_resp.expires_in);

        loop {
            if std::time::Instant::now() > deadline {
                return Err(PimError::Auth("Device code flow timed out".to_string()).into());
            }

            tokio::time::sleep(interval).await;

            let resp = self
                .client
                .post(&token_url)
                .form(&[
                    ("client_id", self.client_id.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &dc_resp.device_code),
                ])
                .send()
                .await?;

            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            if status.is_success() {
                let token_resp: TokenResponse =
                    serde_json::from_str(&body).context("Failed to parse token response")?;

                self.send_status("Graph API authenticated".to_string());
                return Ok(CachedToken {
                    access_token: token_resp.access_token,
                    refresh_token: token_resp.refresh_token,
                    expires_at: now_secs() + token_resp.expires_in,
                });
            }

            // Check if still waiting for user
            if let Ok(err_resp) = serde_json::from_str::<TokenErrorResponse>(&body) {
                match err_resp.error.as_str() {
                    "authorization_pending" => {
                        debug!("Waiting for user to authenticate...");
                        continue;
                    }
                    "slow_down" => {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                    "expired_token" => {
                        return Err(
                            PimError::Auth("Device code expired. Try again.".to_string()).into(),
                        );
                    }
                    _ => {
                        return Err(PimError::Auth(format!(
                            "Device code flow failed: {} - {}",
                            err_resp.error,
                            err_resp.error_description.unwrap_or_default()
                        ))
                        .into());
                    }
                }
            }

            return Err(PimError::Auth(format!("Unexpected response: {body}")).into());
        }
    }
}
