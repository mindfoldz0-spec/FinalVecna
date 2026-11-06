//! Speech analysis of the caller's recorded message:
//!   1. STT — Sarvam (primary). Gnani runs ONLY as a fallback when Sarvam
//!      errors, times out, or returns an empty transcript (never alternated).
//!      Sarvam takes the webm message; Gnani cannot read webm, so the caller
//!      also uploads a WAV sidecar (`message_wav`) built from the same PCM.
//!   2. Triage — the transcript goes to a Qwen model on Groq, which returns
//!      JSON: {category, label, urgency 0-3, summary, keywords, flags}.
//!      Result is stored in `analyses` (kind='triage'), cached on the call,
//!      and broadcast to officers as TRIAGE_UPDATE.
//!
//! Keys come from env (SARVAM_API_KEY / GNANI_API_KEY / GROQ_API_KEY /
//! GROQ_MODEL) with a manual fallback that parses the repo-root `.env`
//! colon-format lines (`sarvav:...`, `gnani:...`, `groq:...`), which
//! dotenvy does not load.

use serde::Deserialize;
use tracing::{info, warn};

use crate::calls::{now_secs, TriageView};
use crate::AppState;

const SARVAM_STT_URL: &str = "https://api.sarvam.ai/speech-to-text";
const GNANI_STT_URL: &str = "https://api.vachana.ai/stt/v3";
const GROQ_CHAT_URL: &str = "https://api.groq.com/openai/v1/chat/completions";

const CATEGORIES: &[&str] = &[
    "cybercrime-financial",
    "cybercrime-fraud-impersonation",
    "cybercrime-data-privacy",
    "image-based-abuse",
    "cybercrime-other",
    "harassment",
    "physical-crime",
    "accident-medical",
    "non-crime",
    "unclear",
];

const FLAGS: &[&str] = &[
    "csam",
    "minor",
    "immediate-danger",
    "financial-loss",
    "ongoing",
];

#[derive(Clone)]
pub struct AnalysisConfig {
    pub sarvam_key: String,
    pub gnani_key: String,
    pub groq_key: String,
    pub groq_model: String,
    pub client: reqwest::Client,
}

/// Env var first; if unset/empty, scan .env files for a `colon_key:value` line
/// (the project stores API keys that way; dotenvy only reads KEY=VALUE).
fn env_or_dotenv_colon(env_name: &str, colon_key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_name) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    let manifest = env!("CARGO_MANIFEST_DIR");
    for base in [
        std::path::Path::new(manifest).to_path_buf(),
        std::path::Path::new(manifest).join(".."),
    ] {
        let p = base.join(".env");
        if let Ok(content) = std::fs::read_to_string(&p) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some(idx) = line.find(':') {
                    if line[..idx].trim() == colon_key {
                        let v = line[idx + 1..].trim();
                        if !v.is_empty() {
                            return Some(v.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

impl AnalysisConfig {
    pub fn from_env() -> Self {
        let sarvam_key = env_or_dotenv_colon("SARVAM_API_KEY", "sarvav").unwrap_or_default();
        let gnani_key = env_or_dotenv_colon("GNANI_API_KEY", "gnani").unwrap_or_default();
        let groq_key = env_or_dotenv_colon("GROQ_API_KEY", "groq").unwrap_or_default();
        let groq_model = std::env::var("GROQ_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "qwen/qwen3.8-27b".to_string());
        if sarvam_key.is_empty() && gnani_key.is_empty() {
            warn!("no STT keys (SARVAM_API_KEY/GNANI_API_KEY) — message transcription disabled");
        }
        if groq_key.is_empty() {
            warn!("no GROQ_API_KEY — transcript triage disabled");
        }
        Self {
            sarvam_key,
            gnani_key,
            groq_key,
            groq_model,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(40))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    pub fn stt_ready(&self) -> bool {
        !self.sarvam_key.is_empty() || !self.gnani_key.is_empty()
    }

    pub fn triage_ready(&self) -> bool {
        !self.groq_key.is_empty()
    }
}

#[derive(Deserialize)]
struct SarvamStt {
    transcript: Option<String>,
    language_code: Option<String>,
}

#[derive(Deserialize)]
struct GnaniStt {
    success: Option<bool>,
    transcript: Option<String>,
    detected_language_code: Option<String>,
}

async fn stt_sarvam(cfg: &AnalysisConfig, bytes: Vec<u8>, filename: &str, mime: &str) -> Result<(String, String), String> {
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str(mime)
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new()
        .text("model", "saaras:v4")
        .text("language_code", "unknown")
        .part("file", part);
    let resp = cfg
        .client
        .post(SARVAM_STT_URL)
        .header("api-subscription-key", &cfg.sarvam_key)
        .timeout(std::time::Duration::from_secs(35))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("sarvam send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("sarvam status {status}: {}", &body[..body.len().min(200)]));
    }
    let out: SarvamStt = resp.json().await.map_err(|e| format!("sarvam parse: {e}"))?;
    let text = out.transcript.unwrap_or_default();
    if text.trim().is_empty() {
        return Err("sarvam returned an empty transcript".to_string());
    }
    Ok((text, out.language_code.unwrap_or_default()))
}

async fn stt_gnani(cfg: &AnalysisConfig, bytes: Vec<u8>, filename: &str) -> Result<(String, String), String> {
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new()
        .text("model", "prisma-v3.0")
        .text("language_code", "indic-auto")
        .text("format", "transcribe")
        .part("audio_file", part);
    let resp = cfg
        .client
        .post(GNANI_STT_URL)
        .header("X-API-Key-ID", &cfg.gnani_key)
        .timeout(std::time::Duration::from_secs(35))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("gnani send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("gnani status {status}: {}", &body[..body.len().min(200)]));
    }
    let out: GnaniStt = resp.json().await.map_err(|e| format!("gnani parse: {e}"))?;
    if out.success == Some(false) {
        return Err("gnani reported success=false".to_string());
    }
    let text = out.transcript.unwrap_or_default();
    if text.trim().is_empty() {
        return Err("gnani returned an empty transcript".to_string());
    }
    Ok((text, out.detected_language_code.unwrap_or_default()))
}

/// Transcribe the message: Sarvam first; Gnani only if Sarvam failed/empty.
/// Returns (provider, language, text).
async fn transcribe(cfg: &AnalysisConfig, webm: &[u8], webm_name: &str, wav: Option<(&[u8], &str)>) -> Result<(String, String, String), String> {
    if !cfg.sarvam_key.is_empty() {
        match stt_sarvam(cfg, webm.to_vec(), webm_name, "audio/webm").await {
            Ok((text, lang)) => return Ok(("sarvam".to_string(), lang, text)),
            Err(e) => warn!("sarvam STT failed, falling back to gnani: {e}"),
        }
    }
    if let Some((wav_bytes, wav_name)) = wav {
        if !cfg.gnani_key.is_empty() {
            match stt_gnani(cfg, wav_bytes.to_vec(), wav_name).await {
                Ok((text, lang)) => return Ok(("gnani".to_string(), lang, text)),
                Err(e) => return Err(format!("gnani fallback also failed: {e}")),
            }
        }
        return Err("sarvam failed and no gnani key for fallback".to_string());
    }
    Err("sarvam failed and no WAV sidecar for the gnani fallback".to_string())
}

#[derive(Deserialize)]
struct GroqChat {
    choices: Vec<GroqChoice>,
}

#[derive(Deserialize)]
struct GroqChoice {
    message: GroqMessage,
}

#[derive(Deserialize)]
struct GroqMessage {
    content: String,
}

#[derive(Deserialize)]
struct TriageJson {
    category: Option<String>,
    label: Option<String>,
    urgency: Option<serde_json::Value>,
    summary: Option<String>,
    keywords: Option<Vec<String>>,
    flags: Option<Vec<String>>,
}

fn triage_prompt(transcript: &str) -> String {
    format!(
        r#"You are triaging an emergency-call message transcript (may be English, Hindi, Hinglish, or any Indian language, possibly with STT errors).

Classify it and respond with ONLY a JSON object, no markdown, no commentary:
{{
  "category": one of [{cats}],
  "label": a short human-readable label of what is happening (max ~8 words),
  "urgency": integer 0-3 (0 = routine/no emergency, 1 = needs follow-up, 2 = serious, 3 = life-threatening or crime in progress / large financial loss / sexual abuse),
  "summary": one sentence describing the situation in plain language,
  "keywords": up to 6 key phrases from the transcript,
  "flags": subset of [{flags}] that apply
}}

Category guidance:
- image-based-abuse: sexual images/videos of the caller shared or threatened to be shared ("send more photos", morphed images, blackmail with photos). If the images involve a minor, add flags csam + minor.
- cybercrime-financial: bank/UPI/OTP/card fraud, money stolen.
- cybercrime-fraud-impersonation: fake police/CBI/court calls, KYC suspension scams, impersonation, sextortion threats.
- cybercrime-data-privacy: hacked accounts, data leak, unauthorized access.
- cybercrime-other: any other digital/cybercrime (phishing, ransomware, online gambling scam, etc.).
- harassment: stalking, threatening calls/messages, abuse without an image/financial element.
- physical-crime: assault, theft, violence, threats of physical harm, kidnapping.
- accident-medical: accidents, injuries, medical emergency.
- non-crime: complaint, lost item, inquiry, prank.
- unclear: transcript too garbled to classify.

Transcript: "{transcript}""#,
        cats = CATEGORIES.join(" | "),
        flags = FLAGS.join(" | "),
    )
}

async fn triage_transcript(cfg: &AnalysisConfig, transcript: &str) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "model": cfg.groq_model,
        "temperature": 0.2,
        "max_tokens": 600,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": "You are an emergency-call triage analyst. Reply with JSON only, exactly matching the requested schema." },
            { "role": "user", "content": triage_prompt(transcript) },
        ],
    });
    let resp = cfg
        .client
        .post(GROQ_CHAT_URL)
        .header("Authorization", format!("Bearer {}", cfg.groq_key))
        .header("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("groq send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("groq status {status}: {}", &text[..text.len().min(200)]));
    }
    let out: GroqChat = resp.json().await.map_err(|e| format!("groq parse: {e}"))?;
    let content = out
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .ok_or("groq returned no choices")?;
    let parsed: TriageJson = serde_json::from_str(&content)
        .map_err(|e| format!("groq content not JSON: {e} — content: {}", &content[..content.len().min(200)]))?;
    let urgency = parsed
        .urgency
        .and_then(|u| u.as_i64().or_else(|| u.as_f64().map(|f| f as i64)))
        .unwrap_or(1)
        .clamp(0, 3);
    let category = parsed
        .category
        .filter(|c| CATEGORIES.contains(&c.as_str()))
        .unwrap_or_else(|| "unclear".to_string());
    let flags: Vec<String> = parsed
        .flags
        .unwrap_or_default()
        .into_iter()
        .filter(|f| FLAGS.contains(&f.as_str()))
        .collect();
    Ok(serde_json::json!({
        "category": category,
        "label": parsed.label.unwrap_or_else(|| "Unclassified message".to_string()),
        "urgency": urgency,
        "summary": parsed.summary.unwrap_or_default(),
        "keywords": parsed.keywords.unwrap_or_default(),
        "flags": flags,
    }))
}

/// Full pipeline for an uploaded message recording. Spawned by recordings.rs —
/// the upload response never waits on STT/LLM inference.
pub async fn run_message_analysis(
    state: &AppState,
    call_id: &str,
    webm_path: &std::path::Path,
    wav_path: Option<&std::path::Path>,
    recording_id: Option<&str>,
) {
    let cfg = &state.analysis;
    if !cfg.stt_ready() {
        return;
    }
    let webm_bytes = match tokio::fs::read(webm_path).await {
        Ok(b) => b,
        Err(e) => {
            warn!("stt: cannot read {}: {e}", webm_path.display());
            return;
        }
    };
    let webm_name = webm_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "message.webm".to_string());
    let wav: Option<(Vec<u8>, String)> = match wav_path {
        Some(p) => match tokio::fs::read(p).await {
            Ok(b) => Some((
                b,
                p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "message_wav.wav".to_string()),
            )),
            Err(e) => {
                warn!("stt: cannot read wav sidecar {}: {e}", p.display());
                None
            }
        },
        None => None,
    };
    let wav_ref = wav.as_ref().map(|(b, n)| (b.as_slice(), n.as_str()));
    let (provider, language, text) = match transcribe(cfg, &webm_bytes, &webm_name, wav_ref).await {
        Ok(v) => v,
        Err(e) => {
            warn!("stt failed for call {call_id}: {e}");
            return;
        }
    };
    info!("stt {call_id}: provider={provider} lang={language} text={:.80}", text.trim());
    if let Some(rid) = recording_id {
        crate::db::record_transcript(&state.db, rid, if language.is_empty() { None } else { Some(&language) }, &text, None, &provider).await;
    }
    crate::db::log_event(
        &state.db,
        call_id,
        None,
        "TRANSCRIPT",
        Some(&serde_json::json!({ "provider": provider, "language": language, "text": text })),
    )
    .await;

    if !cfg.triage_ready() {
        return;
    }
    let triage = match triage_transcript(cfg, &text).await {
        Ok(v) => v,
        Err(e) => {
            warn!("triage failed for call {call_id}: {e}");
            return;
        }
    };
    let view = TriageView {
        category: triage["category"].as_str().unwrap_or("unclear").to_string(),
        label: triage["label"].as_str().unwrap_or_default().to_string(),
        urgency: triage["urgency"].as_i64().unwrap_or(1),
        summary: triage["summary"].as_str().unwrap_or_default().to_string(),
        keywords: triage["keywords"]
            .as_array()
            .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        flags: triage["flags"]
            .as_array()
            .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        provider: "groq".to_string(),
        at: now_secs(),
    };
    if let Some(rid) = recording_id {
        crate::db::record_analysis(&state.db, rid, "triage", "groq", Some(&cfg.groq_model), &triage).await;
    }
    state.calls.set_triage(call_id, view.clone());
    crate::signaling::broadcast_agents(
        &state,
        &serde_json::json!({
            "event": "TRIAGE_UPDATE",
            "call_id": call_id,
            "category": view.category,
            "label": view.label,
            "urgency": view.urgency,
            "summary": view.summary,
            "keywords": view.keywords,
            "flags": view.flags,
            "transcript": text,
            "stt_provider": provider,
            "at": view.at,
        }),
    )
    .await;
    info!(
        "triage {call_id}: {} / {} urgency={} flags={:?}",
        view.category, view.label, view.urgency, view.flags
    );
}
