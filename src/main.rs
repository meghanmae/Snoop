//! Snoop - a local stand-in for Appfire Flow's activity dashboard.
//!
//! Run snoop.exe: it serves the dashboard on 127.0.0.1, opens it in the
//! browser and fetches from Azure DevOps. Everything is set from the page:
//! Settings edits snoop.config.json and signs in, Refresh fetches again.
//! Close the console window to stop it.

mod ado;
mod collect;
mod config;
mod identity;
mod stats;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

const TEMPLATE: &str = include_str!("../template.html");
const SETTINGS_PAGE: &str = include_str!("../settings.html");

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
    /// Settings were saved mid-refresh: go again with them when this one ends.
    queued: bool,
    started: Option<Instant>,
    generated: Option<String>,
    error: Option<String>,
    auth_error: bool,
}

struct App {
    config_path: PathBuf,
    /// The dashboard with the latest data baked in.
    page: RwLock<Option<String>>,
    /// Names seen in the last fetch, to suggest on the settings page.
    names: RwLock<Vec<String>>,
    /// Work item states seen in the last fetch, in ADO's order.
    states: RwLock<Vec<String>>,
    status: Mutex<Status>,
    requests: Arc<AtomicUsize>,
}

impl App {
    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts a refresh, or queues one if a refresh is already running.
    fn refresh(self: &Arc<Self>) {
        {
            let mut status = self.status();
            if status.running {
                status.queued = true;
                return;
            }
            status.running = true;
            status.queued = false;
            status.started = Some(Instant::now());
            status.error = None;
            status.auth_error = false;
        }
        self.requests.store(0, Ordering::Relaxed);

        let app = Arc::clone(self);
        tokio::spawn(async move {
            let result = run_refresh(&app).await;
            let again = {
                let mut status = app.status();
                status.running = false;
                match result {
                    Ok(generated) => status.generated = Some(generated),
                    Err(err) => {
                        println!("ERROR: {err:#}");
                        status.auth_error = ado::is_auth_error(&err);
                        status.error = Some(format!("{err:#}"));
                    }
                }
                status.queued
            };
            if again {
                app.refresh();
            }
        });
    }

    fn status_json(&self) -> Value {
        let status = self.status();
        json!({
            "running": status.running,
            "requests": self.requests.load(Ordering::Relaxed),
            "seconds": status.started.map(|s| s.elapsed().as_secs()).unwrap_or(0),
            "generated": status.generated,
            "error": status.error,
            "authError": status.auth_error,
        })
    }

    fn needs_setup(&self) -> bool {
        config::read(&self.config_path).map_or(true, |c| c.missing().is_some())
    }
}

async fn run_refresh(app: &App) -> Result<String> {
    let settings = config::load(&app.config_path)?;
    if let Some(missing) = settings.config.missing() {
        bail!("Snoop needs {missing} before it can fetch anything. Open Settings to fill it in.");
    }
    let client = ado::Ado::connect(settings.config.max_concurrent_requests, Arc::clone(&app.requests)).await?;
    let payload = collect::refresh(&client, &settings).await?;
    let generated = payload["meta"]["run"].to_string();

    let mut names = BTreeSet::new();
    for list in ["people", "unmatched"] {
        for entry in payload[list].as_array().into_iter().flatten() {
            if let Some(name) = entry["name"].as_str().filter(|n| !n.is_empty()) {
                names.insert(name.to_string());
            }
        }
    }
    *app.names.write().await = names.into_iter().collect();
    *app.states.write().await = payload["stats"]["states"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["n"].as_str().map(str::to_string))
        .collect();

    // Keep a stray "</script>" inside a commit message from ending the script
    // block. "<" is a valid escape inside a JSON string.
    let json = serde_json::to_string(&payload)?.replace('<', "\\u003c");
    *app.page.write().await = Some(TEMPLATE.replace("__SNOOP_DATA__", &json));
    Ok(generated)
}

// --- pages ----------------------------------------------------------------------

async fn index(State(app): State<Arc<App>>) -> Response {
    if let Some(page) = app.page.read().await.as_ref() {
        return Html(page.clone()).into_response();
    }
    let idle = { !app.status().running };
    if idle && app.needs_setup() {
        return Redirect::to("/settings").into_response();
    }
    Html(LOADING_PAGE).into_response()
}

async fn settings_page() -> Html<&'static str> {
    Html(SETTINGS_PAGE)
}

// --- api ------------------------------------------------------------------------

/// A message the page shows as-is.
fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn fail_from(err: &anyhow::Error) -> Response {
    let status = if ado::is_auth_error(err) { StatusCode::UNAUTHORIZED } else { StatusCode::BAD_GATEWAY };
    (status, Json(json!({ "error": format!("{err:#}"), "authError": ado::is_auth_error(err) }))).into_response()
}

async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.status_json())
}

async fn refresh(State(app): State<Arc<App>>) -> Json<Value> {
    app.refresh();
    Json(app.status_json())
}

async fn get_config(State(app): State<Arc<App>>) -> Response {
    match config::read(&app.config_path) {
        Ok(config) => Json(json!({
            "path": app
                .config_path
                .canonicalize()
                .unwrap_or_else(|_| app.config_path.clone())
                .display()
                .to_string()
                .trim_start_matches(r"\\?\")
                .to_string(),
            "config": config,
            "missing": config.missing(),
        }))
        .into_response(),
        Err(err) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")),
    }
}

async fn put_config(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Response {
    let config: config::Config = match serde_json::from_value(body) {
        Ok(config) => config,
        Err(err) => return fail(StatusCode::BAD_REQUEST, format!("Those settings don't fit: {err}")),
    };
    if let Some(missing) = config.missing() {
        return fail(StatusCode::BAD_REQUEST, format!("Snoop needs {missing}."));
    }
    if let Err(err) = config::write(&app.config_path, &config) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"));
    }
    println!("Settings saved to {}", app.config_path.display());
    app.refresh();
    Json(app.status_json()).into_response()
}

/// Reads settings the way a save would, without saving: fills in defaults and
/// tidies the legacy shapes, for importing and exporting a config file.
async fn check_config(Json(body): Json<Value>) -> Response {
    match serde_json::from_value::<config::Config>(body) {
        Ok(config) => Json(json!({ "config": config, "missing": config.missing() })).into_response(),
        Err(err) => fail(StatusCode::BAD_REQUEST, format!("Those settings don't fit: {err}")),
    }
}

async fn auth_status() -> Json<Value> {
    Json(ado::auth_status().await)
}

async fn az_login(State(app): State<Arc<App>>) -> Response {
    match ado::az_login().await {
        Ok(()) => {
            println!("Signed in with az login");
            if !app.needs_setup() {
                app.refresh();
            }
            Json(ado::auth_status().await).into_response()
        }
        Err(err) => fail(StatusCode::BAD_GATEWAY, format!("{err:#}")),
    }
}

#[derive(Deserialize)]
struct PatBody {
    pat: String,
}

async fn save_pat(Json(body): Json<PatBody>) -> Response {
    match ado::save_pat(&body.pat) {
        Ok(()) => Json(ado::auth_status().await).into_response(),
        Err(err) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not save the token: {err:#}")),
    }
}

#[derive(Deserialize)]
struct Lookup {
    org: String,
    #[serde(default)]
    project: String,
}

async fn lookup_client() -> Result<ado::Ado> {
    ado::Ado::connect(4, Arc::new(AtomicUsize::new(0))).await
}

fn names_of(list: Vec<Value>, key: &str) -> Vec<String> {
    let mut names: Vec<String> = list.iter().filter_map(|v| v[key].as_str().map(str::to_string)).collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Projects in an org - also the settings page's "test connection".
async fn lookup_projects(Query(q): Query<Lookup>) -> Response {
    let result = async {
        let client = lookup_client().await?;
        let url = format!("https://dev.azure.com/{}/_apis/projects?$top=500&api-version=7.1", ado::escape(q.org.trim()));
        let projects = client.get(&url).await.map_err(|err| {
            if !ado::is_auth_error(&err) && format!("{err}").contains("404") {
                anyhow::anyhow!("There's no organization called '{}', or you can't see it.", q.org.trim())
            } else {
                err
            }
        })?;
        anyhow::Ok(names_of(ado::values(projects), "name"))
    }
    .await;
    match result {
        Ok(names) => Json(json!(names)).into_response(),
        Err(err) => fail_from(&err),
    }
}

async fn lookup_repos(Query(q): Query<Lookup>) -> Response {
    let result = async {
        let client = lookup_client().await?;
        let url = format!(
            "https://dev.azure.com/{}/{}/_apis/git/repositories?api-version=7.1",
            ado::escape(q.org.trim()),
            ado::escape(q.project.trim())
        );
        anyhow::Ok(names_of(ado::values(client.get(&url).await?), "name"))
    }
    .await;
    match result {
        Ok(names) => Json(json!(names)).into_response(),
        Err(err) => fail_from(&err),
    }
}

/// Area paths, written the way WIQL and the config want them: "Project\Team".
async fn lookup_areas(Query(q): Query<Lookup>) -> Response {
    fn walk(node: &Value, path: String, out: &mut Vec<String>) {
        for child in node["children"].as_array().into_iter().flatten() {
            let child_path = format!("{path}\\{}", child["name"].as_str().unwrap_or(""));
            out.push(child_path.clone());
            walk(child, child_path, out);
        }
    }
    let result = async {
        let client = lookup_client().await?;
        let url = format!(
            "https://dev.azure.com/{}/{}/_apis/wit/classificationnodes/Areas?$depth=10&api-version=7.1",
            ado::escape(q.org.trim()),
            ado::escape(q.project.trim())
        );
        let root = client.get(&url).await?;
        let mut out = vec![q.project.trim().to_string()];
        walk(&root, q.project.trim().to_string(), &mut out);
        anyhow::Ok(out)
    }
    .await;
    match result {
        Ok(names) => Json(json!(names)).into_response(),
        Err(err) => fail_from(&err),
    }
}

async fn lookup_people(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(*app.names.read().await))
}

async fn lookup_states(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(*app.states.read().await))
}

// --- startup --------------------------------------------------------------------

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
    let port = args.port.unwrap_or_else(|| config::read(&config_path).map(|c| c.port).unwrap_or(7414));

    // A second copy, or anything else on the port: take whatever is free.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let url = format!("http://{}/", listener.local_addr()?);

    let app = Arc::new(App {
        config_path: config_path.clone(),
        page: RwLock::new(None),
        names: RwLock::new(vec![]),
        states: RwLock::new(vec![]),
        status: Mutex::new(Status::default()),
        requests: Arc::new(AtomicUsize::new(0)),
    });

    println!("Snoop  {}", env!("CARGO_PKG_VERSION"));
    println!("Config:    {}", config_path.display());
    println!("Dashboard: {url}   (close this window to stop)");

    // Nothing to fetch until the basics are set, so start on the settings page.
    let first_page = if app.needs_setup() {
        println!("Finish setting up in the browser.");
        format!("{url}settings")
    } else {
        app.refresh();
        url.clone()
    };
    if args.open {
        open_browser(&first_page);
    }

    let router = Router::new()
        .route("/", get(index))
        .route("/settings", get(settings_page))
        .route("/api/status", get(status))
        .route("/api/refresh", post(refresh))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/check", post(check_config))
        .route("/api/auth", get(auth_status))
        .route("/api/auth/az-login", post(az_login))
        .route("/api/auth/pat", post(save_pat))
        .route("/api/lookup/projects", get(lookup_projects))
        .route("/api/lookup/repos", get(lookup_repos))
        .route("/api/lookup/areas", get(lookup_areas))
        .route("/api/lookup/people", get(lookup_people))
        .route("/api/lookup/states", get(lookup_states))
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
/// A failure offers the fix: sign in, or go to settings.
const LOADING_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Snoop</title>
<style>
  html, body { margin: 0; height: 100%; background: #0b0d10; color: #c7d2de; font: 13px/1.5 "Segoe UI", system-ui, sans-serif; }
  .box { position: absolute; top: 40%; left: 50%; transform: translate(-50%, -50%); text-align: center; width: min(70ch, 100% - 32px); }
  .brand { font-size: 15px; font-weight: 600; letter-spacing: .12em; color: #e6edf5; margin-bottom: 14px; }
  #msg { color: #7a8794; white-space: pre-wrap; }
  #msg.error { color: #fc8181; text-align: left; }
  .actions { margin-top: 18px; display: flex; gap: 8px; justify-content: center; flex-wrap: wrap; }
  button, a.button { background: #161a1f; color: #e6edf5; border: 1px solid #2e3843; border-radius: 4px; padding: 6px 16px; font: inherit; cursor: pointer; text-decoration: none; }
  button.primary { background: #1d4f8a; border-color: #3987e5; }
  button:disabled { color: #7a8794; cursor: default; }
</style></head>
<body><div class="box"><div class="brand">SNOOP</div><div id="msg">Fetching from Azure DevOps...</div>
<div class="actions" id="actions" hidden>
  <button class="primary" id="signin" hidden>Sign in with Azure CLI</button>
  <button id="retry">Retry</button>
  <a class="button" href="/settings">Settings</a>
</div></div>
<script>
const msg = document.getElementById('msg'), actions = document.getElementById('actions');
const retry = document.getElementById('retry'), signin = document.getElementById('signin');
async function poll() {
  try {
    const s = await (await fetch('/api/status')).json();
    if (s.generated && !s.running) { location.reload(); return; }
    if (s.error && !s.running) {
      msg.className = 'error';
      msg.textContent = s.error;
      actions.hidden = false;
      signin.hidden = !s.authError;
      return;
    }
    msg.className = '';
    actions.hidden = true;
    msg.textContent = `Fetching from Azure DevOps... ${s.requests} requests, ${s.seconds}s`;
  } catch (e) { }
  setTimeout(poll, 700);
}
retry.addEventListener('click', async () => {
  await fetch('/api/refresh', { method: 'POST' });
  poll();
});
signin.addEventListener('click', async () => {
  signin.disabled = true;
  msg.className = '';
  msg.textContent = 'Finish signing in in the browser window that just opened...';
  const r = await fetch('/api/auth/az-login', { method: 'POST' });
  signin.disabled = false;
  if (!r.ok) { msg.className = 'error'; msg.textContent = (await r.json()).error; return; }
  poll();
});
poll();
</script></body></html>"#;
