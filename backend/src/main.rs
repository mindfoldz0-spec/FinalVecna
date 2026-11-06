use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tower_http::services::ServeDir;
use tracing::info;

mod analysis;
mod api;
mod audio;
mod audio_files;
mod auth;
mod calls;
mod db;
mod emotion;
mod recordings;
mod rtc;
mod signaling;

pub use calls::CallManager;

#[derive(Clone)]
pub struct AppState {
    pub calls: Arc<CallManager>,
    pub agent_password: String,
    pub call_token_secret: String,
    pub db: crate::db::Store,
    pub emotion: emotion::EmotionConfig,
    /// call_id -> live PCM buffering state for the emotion analyzer
    pub live_emotions: emotion::LiveMapArc,
    /// STT (Sarvam/Gnani) + LLM triage (Groq) config for message recordings
    pub analysis: analysis::AnalysisConfig,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    dotenvy::dotenv().ok();

    let agent_password = match std::process::Command::new("aws")
        .args([
            "ssm", "get-parameter",
            "--name", "/vecna/officer/password",
            "--with-decryption",
            "--query", "Parameter.Value",
            "--output", "text",
            "--region", "ap-southeast-2",
        ])
        .output() {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !v.is_empty() && v != "None" { v } else { std::env::var("AGENT_PASSWORD").unwrap_or_else(|_| "Officer".to_string()) }
        }
        _ => std::env::var("AGENT_PASSWORD").unwrap_or_else(|_| "Officer".to_string()),
    };
    if agent_password == "Officer" {
        tracing::warn!("AGENT_PASSWORD is the built-in default 'Officer' — set a real one in .env");
    }
    let call_token_secret =
        std::env::var("CALL_TOKEN_SECRET").unwrap_or_else(|_| "change-me-secret".to_string());
    if call_token_secret == "change-me-secret" {
        tracing::warn!("CALL_TOKEN_SECRET is the built-in default — set a real one in .env");
    }

    // AWS persistence: DynamoDB (records) + S3 (audio) + SSM (password).
    // No DATABASE_URL needed; tables + bucket + parameter are provisioned.
    let db = db::connect().await;
    db::seed_agents(&db, &auth::AGENTS, &agent_password).await;

    let state = AppState {
        calls: Arc::new(CallManager::new()),
        agent_password,
        call_token_secret,
        db,
        emotion: emotion::EmotionConfig::from_env(),
        live_emotions: Arc::new(dashmap::DashMap::new()),
        analysis: analysis::AnalysisConfig::from_env(),
    };

    // Periodically drop ended calls from memory (DB keeps them) and sweep
    // stale calls whose caller disappeared without disconnecting cleanly.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                state.calls.purge_ended(300.0);
                for call_id in state.calls.purge_stale(180.0) {
                    db::record_call_status(&state.db, &call_id, None, "ENDED", "STALE_CALL_PURGED").await;
                    signaling::broadcast_agents(&state, &serde_json::json!({ "event": "CALL_ENDED", "call_id": call_id })).await;
                }
            }
        });
    }

    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("frontend")
        .join("dist");

    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/calls", post(signaling::create_call).get(api::list_calls))
        .route("/api/calls/:id", get(api::call_detail))
        .route("/api/rtc-config", get(rtc::rtc_config))
        .route("/api/agent/login", post(auth::agent_login))
        .route("/api/agent/list", get(auth::list_agents))
        .route("/api/agent/me", get(auth::agent_me))
        .route("/audio/:name", get(audio_files::serve))
        .route("/api/audio/manifest", get(audio_files::manifest))
        .route("/ws/caller/:call_id", get(signaling::caller_ws))
        .route("/ws/agent/:agent_id", get(signaling::agent_ws))
        .route(
            "/api/recordings/:call_id",
            post(recordings::upload_recording)
                // axum defaults to a 2MB body limit; long in-call recordings
                // need room. Streaming to disk caps memory, this caps total.
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/*path", get(api_not_found).post(api_not_found))
        .fallback_service(
            ServeDir::new(&dist).not_found_service(tower_http::services::fs::ServeFile::new(dist.join("index.html")))
        )
        .with_state(state)
        .layer(middleware::from_fn(security_headers));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("vecna-server listening on {addr} (static: {})", dist.display());
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown signal received — draining connections");
        })
        .await
        .unwrap();
}

async fn health() -> &'static str {
    "ok"
}

/// Unknown /api/* paths should be honest JSON 404s, not index.html.
async fn api_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({ "error": "not found" })),
    )
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("x-frame-options", "SAMEORIGIN".parse().unwrap());
    h.insert("referrer-policy", "no-referrer".parse().unwrap());
    h.insert(
        "content-security-policy",
        "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self' ws: wss:; media-src 'self' blob:"
            .parse()
            .unwrap(),
    );
    res
}

/// Fixed-window per-IP limiter for the two abusable endpoints only
/// (call creation and officer login). Generous limits: internal tool.
/// Returns Err(429) when over the limit; call at the top of both handlers.
pub fn rate_check(kind: &str, ip: IpAddr) -> Result<(), axum::http::StatusCode> {
    fn store() -> &'static Mutex<HashMap<IpAddr, VecDeque<Instant>>> {
        static STORE: std::sync::OnceLock<Mutex<HashMap<IpAddr, VecDeque<Instant>>>> =
            std::sync::OnceLock::new();
        STORE.get_or_init(|| Mutex::new(HashMap::new()))
    }
    let (max, window): (usize, Duration) = match kind {
        "calls" => (30, Duration::from_secs(60)),
        _ => (20, Duration::from_secs(60)),
    };
    let now = Instant::now();
    let mut map = store().lock().unwrap_or_else(|e| e.into_inner());
    let dq = map.entry(ip).or_default();
    while dq.front().map(|t| now.duration_since(*t) > window).unwrap_or(false) {
        dq.pop_front();
    }
    if dq.len() >= max {
        return Err(axum::http::StatusCode::TOO_MANY_REQUESTS);
    }
    dq.push_back(now);
    Ok(())
}
