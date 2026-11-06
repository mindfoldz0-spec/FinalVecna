//! Live + final speech-emotion analysis.
//!
//! The caller streams raw PCM (16 kHz mono int16) as binary WebSocket frames
//! from the beep until an officer picks the call up. Each time ~2 s of new
//! audio has buffered we POST the window to the self-hosted SER model on AWS
//! (EMOTION_ENDPOINT, see aws/README.md) and broadcast EMOTION_UPDATE to all
//! officer sockets. Scores are EMA-smoothed so the UI doesn't jitter.
//! When the message recording is uploaded we run one final analysis on the
//! whole file (source='final') and persist it against the recording.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use serde::Deserialize;
use tracing::{info, warn};

use crate::calls::{now_secs, CallStatus, EmotionView};
use crate::AppState;

pub const SAMPLE_RATE: u32 = 16000;
/// Analyze once this much new audio has buffered (2 s).
const MIN_NEW_SAMPLES: usize = 2 * SAMPLE_RATE as usize;
/// Cap each inference window (8 s) so latency stays bounded.
const MAX_WINDOW_SAMPLES: usize = 8 * SAMPLE_RATE as usize;
/// Don't let the buffer grow without bound if inference is slow/down.
const MAX_BUF_SAMPLES: usize = 30 * SAMPLE_RATE as usize;
/// EMA smoothing factor for live scores.
const EMA_ALPHA: f64 = 0.45;

#[derive(Clone)]
pub struct EmotionConfig {
    pub endpoint: Option<String>,
    pub api_key: String,
    pub client: reqwest::Client,
}

impl EmotionConfig {
    pub fn from_env() -> Self {
        let endpoint = std::env::var("EMOTION_ENDPOINT")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let api_key = std::env::var("EMOTION_API_KEY").unwrap_or_default();
        if endpoint.is_none() {
            warn!("EMOTION_ENDPOINT not set — live emotion analysis disabled");
        }
        Self {
            endpoint,
            api_key,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(25))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.endpoint.is_some()
    }
}

/// Per-call live streaming state.
pub struct LiveEmo {
    buf: Vec<i16>,
    /// how many samples at the front of `buf` were already sent for analysis
    sent: usize,
    /// one inference in flight per call
    running: AtomicBool,
    /// EMA-smoothed per-label probabilities
    ema: HashMap<String, f64>,
}

pub type LiveMap = DashMap<String, LiveEmo>;

pub fn urgency_of(label: &str) -> i64 {
    match label {
        "fear" | "anger" => 3,
        "sadness" | "disgust" => 2,
        "happiness" | "enthusiasm" | "calm" => 0,
        _ => 1,
    }
}

#[derive(Deserialize)]
struct EmotionOut {
    label: String,
    #[allow(dead_code)]
    confidence: f64,
    scores: serde_json::Value,
    #[allow(dead_code)]
    urgency: i64,
    #[allow(dead_code)]
    duration_s: f64,
    model: Option<String>,
}

/// Stop live analysis for a call (claimed / ended / caller gone).
pub fn stop_live(state: &AppState, call_id: &str) {
    state.live_emotions.remove(call_id);
}

/// Buffer a raw PCM chunk from the caller's socket; kick off an inference
/// window when enough new audio has accumulated. Never blocks the read loop.
pub fn handle_pcm(state: &AppState, call_id: &str, bytes: &[u8]) {
    if !state.emotion.enabled() {
        return;
    }
    // live emotion runs while the call is active (queue + in-call).
    // Pre-claim readings stay memory-only for the queue chip; only
    // post-claim (ENGAGED) in-call readings are persisted.
    match state.calls.get(call_id) {
        Some(c) if matches!(c.status, CallStatus::Ringing | CallStatus::Waiting | CallStatus::Engaged) => {}
        _ => return,
    }
    let samples: Vec<i16> = bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut entry = state
        .live_emotions
        .entry(call_id.to_string())
        .or_insert_with(|| LiveEmo {
            buf: Vec::new(),
            sent: 0,
            running: AtomicBool::new(false),
            ema: HashMap::new(),
        });
    let e = entry.value_mut();
    e.buf.extend_from_slice(&samples);
    if e.buf.len() > MAX_BUF_SAMPLES {
        let excess = e.buf.len() - MAX_BUF_SAMPLES;
        e.buf.drain(..excess);
        e.sent = e.sent.saturating_sub(excess);
    }
    let new_samples = e.buf.len().saturating_sub(e.sent);
    if new_samples >= MIN_NEW_SAMPLES && !e.running.swap(true, Ordering::SeqCst) {
        let end = (e.sent + MAX_WINDOW_SAMPLES).min(e.buf.len());
        let window: Vec<i16> = e.buf[e.sent..end].to_vec();
        e.sent = end;
        drop(entry);
        let state2 = state.clone();
        let cid = call_id.to_string();
        tokio::spawn(async move {
            analyze_live_window(state2, cid, window).await;
        });
    }
}

async fn analyze_pcm(cfg: &EmotionConfig, window: &[i16]) -> Result<EmotionOut, String> {
    let endpoint = cfg.endpoint.as_ref().ok_or("disabled")?;
    let bytes: Vec<u8> = window.iter().flat_map(|s| s.to_le_bytes()).collect();
    let resp = cfg
        .client
        .post(format!("{endpoint}/analyze"))
        .header("X-API-Key", &cfg.api_key)
        .header("X-Sample-Rate", SAMPLE_RATE.to_string())
        .header("Content-Type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("emotion endpoint status {}", resp.status()));
    }
    resp.json::<EmotionOut>().await.map_err(|e| e.to_string())
}

async fn analyze_live_window(state: AppState, call_id: String, window: Vec<i16>) {
    let res = analyze_pcm(&state.emotion, &window).await;
    // release the in-flight slot; if more audio piled up meanwhile, re-kick
    if let Some(e) = state.live_emotions.get(&call_id) {
        e.running.store(false, Ordering::SeqCst);
        let again = e.buf.len().saturating_sub(e.sent) >= MIN_NEW_SAMPLES;
        drop(e);
        if again {
            handle_pcm(&state, &call_id, &[]);
            return;
        }
    }
    let out = match res {
        Ok(v) => v,
        Err(e) => {
            warn!("emotion analyze failed for {call_id}: {e}");
            return;
        }
    };
    // broadcast while the call is live (queue + in-call)
    let engaged = matches!(
        state.calls.get(&call_id),
        Some(c) if matches!(c.status, CallStatus::Engaged)
    );
    match state.calls.get(&call_id) {
        Some(c) if matches!(c.status, CallStatus::Ringing | CallStatus::Waiting | CallStatus::Engaged) => {}
        _ => return,
    }
    // EMA-smooth the score vector so the console doesn't jitter
    let mut ema_map: HashMap<String, f64> = HashMap::new();
    if let Some(mut e) = state.live_emotions.get_mut(&call_id) {
        if let Some(obj) = out.scores.as_object() {
            for (k, p) in obj {
                let p = p.as_f64().unwrap_or(0.0);
                let ema = e.ema.entry(k.clone()).or_insert(p);
                *ema = *ema + EMA_ALPHA * (p - *ema);
            }
        }
        ema_map = e.ema.clone();
    }
    let mut scores = serde_json::Map::new();
    let total: f64 = ema_map.values().sum();
    for (k, v) in &ema_map {
        scores.insert(k.clone(), serde_json::json!(((v / total).max(0.0)).min(1.0)));
    }
    let label = ema_map
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| out.label.clone());
    let confidence = scores
        .get(&label)
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let urgency = urgency_of(&label);
    let scores = serde_json::Value::Object(scores);
    let view = EmotionView {
        label: label.clone(),
        confidence,
        urgency,
        scores: scores.clone(),
        source: "live".into(),
        at: now_secs(),
    };
    state.calls.set_emotion(&call_id, view);
    // broadcast before persisting — the queue chip must not wait on DB latency
    crate::signaling::broadcast_agents(
        &state,
        &serde_json::json!({
            "event": "EMOTION_UPDATE",
            "call_id": call_id,
            "label": label,
            "confidence": confidence,
            "urgency": urgency,
            "scores": scores,
            "source": "live",
        }),
    )
    .await;
    // pre-claim live readings stay memory-only; only in-call (ENGAGED)
    // readings are persisted
    if engaged {
        crate::db::record_emotion_persist(&state.db, &call_id, &label, confidence, &scores, urgency, "live", None, true).await;
    }
    info!("emotion live {call_id}: {label} ({confidence:.2}) urgency {urgency}");
}

/// Final analysis of an uploaded recording (the caller's message). Runs in a
/// spawned task so the upload response never waits on the model.
pub async fn analyze_recording(
    state: &AppState,
    call_id: &str,
    path: &std::path::Path,
    recording_id: Option<&str>,
) {
    if !state.emotion.enabled() {
        return;
    }
    let endpoint = match &state.emotion.endpoint {
        Some(e) => e.clone(),
        None => return,
    };
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => {
            warn!("emotion: read recording for {call_id}: {e}");
            return;
        }
    };
    let fname = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "recording.bin".into());
    let part = reqwest::multipart::Part::bytes(bytes).file_name(fname);
    let form = reqwest::multipart::Form::new().part("audio", part);
    let resp = match state
        .emotion
        .client
        .post(format!("{endpoint}/analyze"))
        .header("X-API-Key", &state.emotion.api_key)
        .multipart(form)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("emotion final call failed for {call_id}: {e}");
            return;
        }
    };
    if !resp.status().is_success() {
        warn!("emotion final endpoint status {} for {call_id}", resp.status());
        return;
    }
    let out: EmotionOut = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("emotion final parse failed for {call_id}: {e}");
            return;
        }
    };
    let urgency = urgency_of(&out.label);
    let view = EmotionView {
        label: out.label.clone(),
        confidence: out.confidence,
        urgency,
        scores: out.scores.clone(),
        source: "final".into(),
        at: now_secs(),
    };
    if let Some(rid) = recording_id {
        crate::db::record_analysis(
            &state.db,
            rid,
            "emotion",
            "ec2-ser",
            out.model.as_deref(),
            &serde_json::json!({
                "label": out.label, "confidence": out.confidence,
                "scores": out.scores, "urgency": urgency,
            }),
        )
        .await;
    }
    state.calls.set_emotion(call_id, view);
    crate::db::record_emotion(
        &state.db,
        call_id,
        &out.label,
        out.confidence,
        &out.scores,
        urgency,
        "final",
        recording_id,
    )
    .await;
    crate::signaling::broadcast_agents(
        &state,
        &serde_json::json!({
            "event": "EMOTION_UPDATE",
            "call_id": call_id,
            "label": out.label,
            "confidence": out.confidence,
            "urgency": urgency,
            "scores": out.scores,
            "source": "final",
        }),
    )
    .await;
    info!("emotion final {call_id}: {} ({:.2}) urgency {urgency}", out.label, out.confidence);
}

/// Helper for tests / debugging.
pub fn live_buffered(state: &AppState, call_id: &str) -> usize {
    state
        .live_emotions
        .get(call_id)
        .map(|e| e.buf.len().saturating_sub(e.sent))
        .unwrap_or(0)
}

pub type LiveMapArc = Arc<LiveMap>;
