//! Azure DevOps over HTTPS. Read-only: GETs plus WIQL / batch POSTs.
//!
//! Everything goes through one client with a cap on requests in flight - the
//! speed comes from running them side by side, and the cap keeps ADO from
//! throttling us. 429s and 5xx are retried, honouring Retry-After.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::{Method, StatusCode, header};
use serde_json::Value;
use tokio::sync::Semaphore;

const ADO_RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";
const ATTEMPTS: u32 = 4;
/// az tokens last an hour; reuse one well inside that.
const TOKEN_REUSE: Duration = Duration::from_secs(30 * 60);
const KEYRING_SERVICE: &str = "snoop";
const KEYRING_USER: &str = "ado-pat";

/// Signing in is what needs fixing, so the page can offer a sign-in button
/// rather than just an error.
#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthError {}

pub fn is_auth_error(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<AuthError>())
}

#[derive(Clone)]
pub struct Ado {
    http: reqwest::Client,
    auth: String,
    gate: Arc<Semaphore>,
    /// Completed requests, for the progress readout.
    pub done: Arc<AtomicUsize>,
}

impl Ado {
    pub async fn connect(max_in_flight: usize, done: Arc<AtomicUsize>) -> Result<Self> {
        let auth = auth_header().await?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .gzip(true)
            .build()?;
        Ok(Self { http, auth, gate: Arc::new(Semaphore::new(max_in_flight.max(1))), done })
    }

    pub async fn get(&self, url: &str) -> Result<Value> {
        self.send(Method::GET, url, None).await
    }

    pub async fn post(&self, url: &str, body: &Value) -> Result<Value> {
        self.send(Method::POST, url, Some(body)).await
    }

    async fn send(&self, method: Method, url: &str, body: Option<&Value>) -> Result<Value> {
        let _permit = self.gate.acquire().await?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut request = self.http.request(method.clone(), url).header(header::AUTHORIZATION, &self.auth);
            if let Some(body) = body {
                request = request.json(body);
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(_) if attempt < ATTEMPTS => {
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt) * 2)).await;
                    continue;
                }
                Err(err) => return Err(anyhow!("{method} {url} failed: {err}")),
            };

            let status = response.status();
            // ADO answers a bad credential with 203 and a sign-in page, not a 401.
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::NON_AUTHORITATIVE_INFORMATION {
                forget_token();
                return Err(AuthError(
                    "Azure DevOps rejected the credentials. Sign in again, or check the organization name. \
                     If the org is in another tenant, use a personal access token instead."
                        .into(),
                )
                .into());
            }
            if status.is_success() {
                self.done.fetch_add(1, Ordering::Relaxed);
                return response.json::<Value>().await.with_context(|| format!("{method} {url}: bad JSON"));
            }
            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) && attempt < ATTEMPTS {
                let wait = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(u64::from(attempt) * 2);
                tokio::time::sleep(Duration::from_secs(wait.min(60))).await;
                continue;
            }
            // ADO explains itself in {"message": ...}; anything else (an HTML
            // error page, say) is noise.
            let text = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_default();
            bail!("{method} {url} failed: {status} {message}");
        }
    }
}

// --- credentials --------------------------------------------------------------

static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Drop the cached credential, after a sign-in change or a rejection.
pub fn forget_token() {
    *TOKEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

fn pat_header(pat: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!(":{}", pat.trim())))
}

fn keyring_entry() -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?)
}

pub fn saved_pat() -> Option<String> {
    keyring_entry().ok()?.get_password().ok().filter(|p| !p.trim().is_empty())
}

/// An empty token removes the saved one.
pub fn save_pat(pat: &str) -> Result<()> {
    let entry = keyring_entry()?;
    if pat.trim().is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(err) => return Err(err.into()),
        }
    } else {
        entry.set_password(pat.trim())?;
    }
    forget_token();
    Ok(())
}

fn env_pat() -> Option<String> {
    std::env::var("ADO_PAT").ok().filter(|p| !p.trim().is_empty())
}

/// ADO_PAT, then a token saved from the settings page, then `az login`.
async fn auth_header() -> Result<String> {
    if let Some((header, at)) = TOKEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
        if at.elapsed() < TOKEN_REUSE {
            return Ok(header);
        }
    }

    let header = if let Some(pat) = env_pat().or_else(saved_pat) {
        pat_header(&pat)
    } else {
        let output = az(&["account", "get-access-token", "--resource", ADO_RESOURCE, "--query", "accessToken", "-o", "tsv"])
            .await?;
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() || token.is_empty() {
            return Err(AuthError(format!(
                "Not signed in to Azure DevOps. Sign in with the Azure CLI, or save a personal access token.\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        format!("Bearer {token}")
    };
    *TOKEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((header.clone(), Instant::now()));
    Ok(header)
}

async fn az(args: &[&str]) -> Result<std::process::Output> {
    az_with(args, &[]).await
}

async fn az_with(args: &[&str], env: &[(&str, &str)]) -> Result<std::process::Output> {
    // `az` is az.cmd on Windows, which only cmd.exe can run.
    #[cfg(windows)]
    let mut command = {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = tokio::process::Command::new("cmd");
        command.arg("/C").arg("az").creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(not(windows))]
    let mut command = tokio::process::Command::new("az");

    command
        .args(args)
        .env("AZURE_CORE_NO_COLOR", "1")
        .env("AZURE_CORE_SURVEY_MESSAGE", "no")
        // The newer interactive account picker waits on stdin, and there is none.
        .env("AZURE_CORE_LOGIN_EXPERIENCE_V2", "no")
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|_| AuthError("Could not run 'az'. Install the Azure CLI, or save a personal access token.".into()).into())
}

/// Which credential the next fetch will use, for the settings page.
pub async fn auth_status() -> Value {
    if env_pat().is_some() {
        return serde_json::json!({ "source": "env", "detail": "Personal access token from the ADO_PAT environment variable" });
    }
    if saved_pat().is_some() {
        return serde_json::json!({ "source": "saved", "detail": "Personal access token saved in Windows Credential Manager" });
    }
    match az(&["account", "show", "--query", "user.name", "-o", "tsv"]).await {
        Ok(output) if output.status.success() => serde_json::json!({
            "source": "az",
            "detail": format!("Azure CLI, signed in as {}", String::from_utf8_lossy(&output.stdout).trim()),
        }),
        Ok(output) => {
            // cmd.exe exits 9009 when the command doesn't exist.
            let installed = output.status.code() != Some(9009);
            serde_json::json!({
                "source": "none",
                "azInstalled": installed,
                "detail": if installed { "Not signed in" } else { "Not signed in, and the Azure CLI isn't installed" },
            })
        }
        Err(_) => serde_json::json!({ "source": "none", "azInstalled": false, "detail": "Not signed in, and the Azure CLI isn't installed" }),
    }
}

/// Interactive `az login`: opens the browser and waits for the user.
pub async fn az_login() -> Result<()> {
    // A windowless process can't host the Windows sign-in dialog; use the browser.
    let run = az_with(
        &["login", "--allow-no-subscriptions", "--output", "none", "--only-show-errors"],
        &[("AZURE_CORE_ENABLE_BROKER_ON_WINDOWS", "false")],
    );
    let output = tokio::time::timeout(Duration::from_secs(300), run)
        .await
        .map_err(|_| anyhow!("Sign-in timed out after 5 minutes."))??;
    forget_token();
    if !output.status.success() {
        bail!("az login failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

// --- helpers ------------------------------------------------------------------

/// Same as [uri]::EscapeDataString: everything but unreserved characters.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The "value" array of a list response, taken by value.
pub fn values(mut response: Value) -> Vec<Value> {
    match response.get_mut("value").map(Value::take) {
        Some(Value::Array(items)) => items,
        _ => vec![],
    }
}

pub fn str_at<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut node = value;
    for key in path {
        node = &node[*key];
    }
    node.as_str().unwrap_or("")
}
