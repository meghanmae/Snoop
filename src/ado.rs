//! Azure DevOps over HTTPS. Read-only: GETs plus WIQL / batch POSTs.
//!
//! Everything goes through one client with a cap on requests in flight - the
//! speed comes from running them side by side, and the cap keeps ADO from
//! throttling us. 429s and 5xx are retried, honouring Retry-After.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use reqwest::{Method, StatusCode, header};
use serde_json::Value;
use tokio::sync::Semaphore;

const ADO_RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";
const ATTEMPTS: u32 = 4;

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
                bail!(
                    "Azure DevOps rejected the credentials. Run 'az login', or if the org is in another \
                     tenant set the ADO_PAT environment variable to a personal access token."
                );
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
            let text = response.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(300).collect();
            bail!("{method} {url} failed: {status} {snippet}");
        }
    }
}

/// $env:ADO_PAT if set, otherwise whoever `az login` is signed in as.
async fn auth_header() -> Result<String> {
    if let Ok(pat) = std::env::var("ADO_PAT") {
        if !pat.trim().is_empty() {
            println!("Auth: personal access token (ADO_PAT)");
            let raw = format!(":{}", pat.trim());
            return Ok(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw)));
        }
    }

    let args = ["account", "get-access-token", "--resource", ADO_RESOURCE, "--query", "accessToken", "-o", "tsv"];
    // `az` is az.cmd on Windows, which only cmd.exe can run.
    #[cfg(windows)]
    let output = {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        tokio::process::Command::new("cmd")
            .arg("/C")
            .arg("az")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .await
    };
    #[cfg(not(windows))]
    let output = tokio::process::Command::new("az").args(args).output().await;

    let output = output.context("could not run 'az' - is the Azure CLI installed?")?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        bail!(
            "Could not get an Azure DevOps token. Run 'az login', or set ADO_PAT.\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    println!("Auth: az login");
    Ok(format!("Bearer {token}"))
}

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
