//! Snoop - a local stand-in for Appfire Flow's activity dashboard.
//!
//! Run snoop.exe: it reads snoop.config.json, serves the dashboard on
//! 127.0.0.1, opens it in the browser and fetches from Azure DevOps. The
//! Refresh button on the page re-reads the config and fetches again; nothing
//! is written anywhere. Close the console window to stop it.

mod ado;
mod collect;
mod config;
mod identity;
mod stats;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::sync::RwLock;

const TEMPLATE: &str = include_str!("../template.html");

const USAGE: &str = "\
snoop.exe [--config <file>] [--port <n>] [--no-open]

  --config <file>   settings file (default: snoop.config.json here, else next to the exe)
  --port <n>        local port for the dashboard (default: \"port\" in the config, 7414)
  --no-open         don't open a browser";

struct Args {
    config: Option<PathBuf>,
    port: Option<u16>,
    open: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { config: None, port: None, open: true };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_lowercase().trim_start_matches('-') {
            "config" | "configfile" => args.config = Some(it.next().context("--config needs a file")?.into()),
            "port" => args.port = Some(it.next().context("--port needs a number")?.parse()?),
            "no-open" | "noopen" => args.open = false,
            "help" | "h" | "?" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}'\n\n{USAGE}"),
        }
    }
    Ok(args)
}

#[derive(Default)]
struct Status {
    running: bool,
    started: Option<Instant>,
    generated: Option<String>,
    error: Option<String>,
}

struct App {
    config_path: PathBuf,
    /// The dashboard with the latest data baked in.
    page: RwLock<Option<String>>,
    status: Mutex<Status>,
    requests: Arc<AtomicUsize>,
}

impl App {
    /// Starts a refresh unless one is already running.
    fn refresh(self: &Arc<Self>) {
        {
            let mut status = self.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if status.running {
                return;
            }
            status.running = true;
            status.started = Some(Instant::now());
            status.error = None;
        }
        self.requests.store(0, Ordering::Relaxed);

        let app = Arc::clone(self);
        tokio::spawn(async move {
            let result = run_refresh(&app).await;
            let mut status = app.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            status.running = false;
            match result {
                Ok(generated) => status.generated = Some(generated),
                Err(err) => {
                    println!("ERROR: {err:#}");
                    status.error = Some(format!("{err:#}"));
                }
            }
        });
    }

    fn status_json(&self) -> Value {
        let status = self.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        json!({
            "running": status.running,
            "requests": self.requests.load(Ordering::Relaxed),
            "seconds": status.started.map(|s| s.elapsed().as_secs()).unwrap_or(0),
            "generated": status.generated,
            "error": status.error,
        })
    }
}

async fn run_refresh(app: &App) -> Result<String> {
    let settings = config::load(&app.config_path)?;
    let client = ado::Ado::connect(settings.config.max_concurrent_requests, Arc::clone(&app.requests)).await?;
    let payload = collect::refresh(&client, &settings).await?;
    let generated = payload["meta"]["run"].to_string();

    // Keep a stray "</script>" inside a commit message from ending the script
    // block. "<" is a valid escape inside a JSON string.
    let json = serde_json::to_string(&payload)?.replace('<', "\\u003c");
    *app.page.write().await = Some(TEMPLATE.replace("__SNOOP_DATA__", &json));
    Ok(generated)
}

async fn index(State(app): State<Arc<App>>) -> impl IntoResponse {
    match app.page.read().await.as_ref() {
        Some(page) => Html(page.clone()),
        None => Html(LOADING_PAGE.to_string()),
    }
}

async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.status_json())
}

async fn refresh(State(app): State<Arc<App>>) -> Json<Value> {
    app.refresh();
    Json(app.status_json())
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).creation_flags(CREATE_NO_WINDOW).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

async fn run() -> Result<()> {
    let args = parse_args()?;
    let config_path = config::locate(args.config)?;
    let settings = config::load(&config_path)?;
    let port = args.port.unwrap_or(settings.config.port);

    // A second copy, or anything else on the port: take whatever is free.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let url = format!("http://{}/", listener.local_addr()?);

    let app = Arc::new(App {
        config_path: config_path.clone(),
        page: RwLock::new(None),
        status: Mutex::new(Status::default()),
        requests: Arc::new(AtomicUsize::new(0)),
    });

    println!("Snoop  {}", env!("CARGO_PKG_VERSION"));
    println!("Config:    {}", config_path.display());
    println!("Dashboard: {url}   (close this window to stop)");

    app.refresh();
    if args.open {
        open_browser(&url);
    }

    let router = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/refresh", post(refresh))
        .with_state(app);
    axum::serve(listener, router).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        println!("ERROR: {err:#}");
        // Keep a double-clicked window open long enough to read why.
        println!("Press Enter to exit");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        std::process::exit(1);
    }
}

/// Shown until the first fetch lands; polls, then reloads into the dashboard.
const LOADING_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Snoop</title>
<style>
  html, body { margin: 0; height: 100%; background: #0b0d10; color: #c7d2de; font: 13px/1.5 "Segoe UI", system-ui, sans-serif; }
  .box { position: absolute; top: 40%; left: 50%; transform: translate(-50%, -50%); text-align: center; max-width: 70ch; padding: 0 16px; }
  .brand { font-size: 15px; font-weight: 600; letter-spacing: .12em; color: #e6edf5; margin-bottom: 14px; }
  #msg { color: #7a8794; white-space: pre-wrap; }
  #msg.error { color: #fc8181; text-align: left; }
  button { margin-top: 16px; background: #161a1f; color: #e6edf5; border: 1px solid #2e3843; border-radius: 4px; padding: 6px 16px; font: inherit; cursor: pointer; }
</style></head>
<body><div class="box"><div class="brand">SNOOP</div><div id="msg">Fetching from Azure DevOps...</div>
<button id="retry" hidden>Retry</button></div>
<script>
const msg = document.getElementById('msg'), retry = document.getElementById('retry');
async function poll() {
  try {
    const s = await (await fetch('/api/status')).json();
    if (s.generated && !s.running) { location.reload(); return; }
    if (s.error && !s.running) {
      msg.className = 'error';
      msg.textContent = s.error + '\n\nFix the problem (or the config file), then retry.';
      retry.hidden = false;
      return;
    }
    msg.className = '';
    msg.textContent = `Fetching from Azure DevOps... ${s.requests} requests, ${s.seconds}s`;
  } catch (e) { }
  setTimeout(poll, 700);
}
retry.addEventListener('click', async () => {
  retry.hidden = true;
  await fetch('/api/refresh', { method: 'POST' });
  poll();
});
poll();
</script></body></html>"#;
