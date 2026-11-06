use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::AppState;

#[derive(Serialize)]
pub struct UploadResponse {
    pub status: &'static str,
    pub call_id: String,
    pub bytes: usize,
}

fn recordings_dir() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("recordings");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// POST /api/recordings/{call_id} — caller token OR agent session.
/// Streams each field straight to disk (no whole-file buffering), so long
/// in-call recordings are fine. The 2MB axum default body limit is lifted
/// for this route in main.rs.
pub async fn upload_recording(
    State(state): State<AppState>,
    Path(call_id): Path<String>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> Result<axum::Json<UploadResponse>, StatusCode> {
    let token_ok = headers
        .get(crate::auth::CALL_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|t| crate::auth::verify_call_token(&state.call_token_secret, &call_id, t))
        .unwrap_or(false);
    let agent_ok = crate::auth::agent_from_headers(&state, &headers).is_some();
    if !token_ok && !agent_ok {
        return Err(StatusCode::FORBIDDEN);
    }
    if state.calls.get(&call_id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let dir = recordings_dir();
    let mut total = 0usize;
    // message upload may arrive with a WAV sidecar (message_wav, built from the
    // live PCM stream) used as the Gnani STT fallback — webm can't be read by Gnani
    let mut message_rec: Option<(std::path::PathBuf, Option<String>)> = None;
    let mut message_wav: Option<std::path::PathBuf> = None;
    while let Ok(Some(mut field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let mime = field.content_type().unwrap_or("application/octet-stream").to_string();
        if !matches!(name.as_str(), "audio" | "caller_audio" | "agent_audio" | "message" | "message_wav") {
            // drain unknown fields so the stream offsets stay correct
            while let Ok(Some(_)) = field.chunk().await {}
            continue;
        }
        let safe = sanitize(&name);
        let side = match name.as_str() {
            "message" | "message_wav" => "message",
            "agent_audio" => "agent",
            "caller_audio" => "caller",
            _ => "system",
        };
        let tmp = dir.join(format!("{call_id}.{safe}.upload"));
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let mut head = [0u8; 4];
        let mut head_done = false;
        let mut n = 0usize;
        while let Some(chunk) = field.chunk().await.map_err(|_| StatusCode::BAD_REQUEST)? {
            if !head_done {
                let take = chunk.len().min(4);
                head[..take].copy_from_slice(&chunk[..take]);
                head_done = true;
            }
            file.write_all(&chunk)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            n += chunk.len();
        }
        drop(file);
        if n == 0 {
            let _ = tokio::fs::remove_file(&tmp).await;
            continue;
        }
        // sniff the real container so recordings keep their true extension
        let ext = if &head == b"RIFF" { "wav" } else { "webm" };
        let dest = dir.join(format!("{call_id}.{safe}.{ext}"));
        tokio::fs::rename(&tmp, &dest)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let filename = dest.file_name().unwrap().to_string_lossy().to_string();
        // WAV probe for dashboards (only worth it for modest files)
        let (rms, zcr) = if ext == "wav" && n < 5_000_000 {
            match tokio::fs::read(&dest).await {
                Ok(data) => match crate::audio::validate_wav(&data) {
                    Ok(_) => {
                        let f = crate::audio::acoustic_features(&data);
                        (Some(f.rms as f64), Some(f.zcr as f64))
                    }
                    Err(_) => (None, None),
                },
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };
        let rid = {
            let key = format!("calls/{call_id}/{filename}");
            crate::db::s3_upload(dest.clone(), key.clone());
            crate::db::record_recording_s3(&state.db, &call_id, side, &filename, n, &mime, rms, zcr, "s3", Some(&key)).await
        };
        // the caller's message recording (webm) gets a final emotion analysis on
        // the AWS model (spawned — the upload response never waits on inference)
        if name == "message" {
            message_rec = Some((dest.clone(), rid.clone()));
            let st = state.clone();
            let cid = call_id.clone();
            let p = dest.clone();
            tokio::spawn(async move {
                crate::emotion::analyze_recording(&st, &cid, &p, rid.as_deref()).await;
            });
        }
        if name == "message_wav" {
            message_wav = Some(dest.clone());
        }
        total += n;
    }
    // STT (Sarvam -> Gnani fallback on the WAV sidecar) + Groq triage of the
    // message transcript, also spawned so the upload responds immediately
    if let Some((webm, rid)) = message_rec {
        let st = state.clone();
        let cid = call_id.clone();
        tokio::spawn(async move {
            crate::analysis::run_message_analysis(&st, &cid, &webm, message_wav.as_deref(), rid.as_deref()).await;
        });
    }
    if total == 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(axum::Json(UploadResponse {
        status: "stored",
        call_id,
        bytes: total,
    }))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .take(32)
        .collect()
}
