use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::AppState;

/// Static IVR audio: greeting, beep, confirmation, hold music.
/// Served from <repo>/resource/audio so voices generated once by
/// scripts/generate-voices.mjs are reused on every call (no per-call API).
fn audio_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("resource")
        .join("audio")
}

fn safe_name(name: &str) -> Option<&str> {
    if name.is_empty()
        || name.len() > 128
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return None;
    }
    Some(name)
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

pub async fn serve(Path(name): Path<String>) -> Response {
    let Some(name) = safe_name(&name) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path = audio_dir().join(name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type(name))
            .header(header::CACHE_CONTROL, "public, max-age=86400")
            .body(axum::body::Body::from(bytes))
            .unwrap(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Which IVR assets exist (and which are still placeholders).
pub async fn manifest(State(_state): State<AppState>) -> Json<serde_json::Value> {
    let dir = audio_dir();
    let expected = ["greeting.mp3", "beep.wav", "confirmation.mp3", "hold_music.mp3", "unlock.wav"];
    let mut files = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if let Ok(md) = e.metadata().await {
                if md.is_file() {
                    files.push(serde_json::json!({
                        "name": e.file_name().to_string_lossy(),
                        "bytes": md.len(),
                        "url": format!("/audio/{}", e.file_name().to_string_lossy()),
                    }));
                }
            }
        }
    }
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(serde_json::json!({
        "dir": dir.display().to_string(),
        "expected": expected,
        "files": files,
    }))
}
