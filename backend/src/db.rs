//! AWS persistence: DynamoDB (records) + S3 (audio) + SSM (officer password).
//!
//! No AWS SDK crates — everything goes through the `aws` CLI so the build
//! stays incremental and fast. All writes are fire-and-forget (spawned, never
//! awaited on the hot path); reads for the officer API come from the
//! in-memory mirrors below so the queue never waits on Dynamo latency.

use dashmap::DashMap;
use std::sync::Arc;

const REGION: &str = "ap-southeast-2";
pub const RECORDINGS_BUCKET: &str = "vecna-recordings-930810028586";

pub fn now_epoch() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn fire(args: Vec<String>) {
    tokio::spawn(async move {
        let _ = tokio::process::Command::new("aws")
            .args(&args)
            .output()
            .await;
    });
}

fn put_item(table: &str, item: serde_json::Value) {
    fire(vec![
        "dynamodb".into(),
        "put-item".into(),
        "--table-name".into(),
        table.into(),
        "--item".into(),
        item.to_string(),
        "--region".into(),
        REGION.into(),
    ]);
}

fn s(v: &str) -> serde_json::Value {
    serde_json::json!({ "S": v })
}
fn sn(v: Option<&str>) -> serde_json::Value {
    match v {
        Some(x) => serde_json::json!({ "S": x }),
        None => serde_json::json!({ "NULL": true }),
    }
}
fn num(v: i64) -> serde_json::Value {
    serde_json::json!({ "N": v.to_string() })
}
fn fnum(v: f64) -> serde_json::Value {
    serde_json::json!({ "N": v.to_string() })
}

#[derive(Clone, Default)]
pub struct Store {
    pub calls: Arc<DashMap<String, serde_json::Value>>,
    pub recordings: Arc<DashMap<String, serde_json::Value>>,
    pub transcripts: Arc<DashMap<String, serde_json::Value>>,
    pub analyses: Arc<DashMap<String, serde_json::Value>>,
    pub events: Arc<DashMap<String, Vec<serde_json::Value>>>,
    pub emotions: Arc<DashMap<String, Vec<serde_json::Value>>>,
    pub agents: Arc<DashMap<String, serde_json::Value>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Boot: no DATABASE_URL needed anymore. Tables already exist.
pub async fn connect() -> Store {
    tracing::info!("aws store ready (dynamodb + s3, region {REGION})");
    Store::new()
}

pub fn s3_upload(local: std::path::PathBuf, key: String) {
    let uri = format!("s3://{RECORDINGS_BUCKET}/{key}");
    let local_s = local.to_string_lossy().to_string();
    fire(vec![
        "s3".into(),
        "cp".into(),
        local_s,
        uri,
        "--region".into(),
        REGION.into(),
    ]);
}

pub async fn seed_agents(store: &Store, agents: &[&str], password: &str) {
    use sha2::{Digest, Sha256};
    for a in agents {
        let hash = hex::encode(Sha256::digest(format!("{a}:{password}").as_bytes()));
        let now = now_rfc3339();
        let ep = now_epoch();
        store.agents.insert(
            a.to_string(),
            serde_json::json!({
                "agent_id": a, "display_name": a, "password_hash": hash,
                "created_at": now, "last_login_at": serde_json::Value::Null,
            }),
        );
        put_item(
            "vecna_agents",
            serde_json::json!({
                "agent_id": s(a),
                "display_name": s(a),
                "password_hash": s(&hash),
                "created_at": s(&now),
                "created_epoch": num(ep),
            }),
        );
    }
}

pub async fn record_call_created(
    store: &Store,
    call_id: &str,
    dialed: &str,
    ip: Option<&str>,
    location: Option<(f64, f64, Option<f64>)>,
) {
    let now = now_rfc3339();
    let ep = now_epoch();
    store.calls.insert(
        call_id.to_string(),
        serde_json::json!({
            "call_id": call_id, "dialed_number": dialed, "status": "RINGING",
            "agent_id": serde_json::Value::Null,
            "caller_ip": ip.unwrap_or_default(),
            "created_at": now, "created_epoch": ep, "shard": "ALL",
            "claimed_at": serde_json::Value::Null, "engaged_at": serde_json::Value::Null,
            "ended_at": serde_json::Value::Null, "ivr_finished_at": serde_json::Value::Null,
            "location": location.map(|(lat, lng, acc)| serde_json::json!({ "lat": lat, "lng": lng, "accuracy": acc })),
        }),
    );
    let mut item = serde_json::json!({
        "call_id": s(call_id),
        "dialed_number": s(dialed),
        "status": s("RINGING"),
        "caller_ip": s(ip.unwrap_or_default()),
        "created_at": s(&now),
        "created_epoch": num(ep),
        "shard": s("ALL"),
    });
    if let Some((lat, lng, acc)) = location {
        let mut m = serde_json::Map::new();
        m.insert("lat".into(), fnum(lat));
        m.insert("lng".into(), fnum(lng));
        if let Some(a) = acc {
            m.insert("accuracy".into(), fnum(a));
        }
        item["location"] = serde_json::json!({ "M": serde_json::Value::Object(m) });
    }
    put_item("vecna_calls", item);
    log_event(store, call_id, None, "CREATED", None).await;
}

pub async fn record_call_status(
    store: &Store,
    call_id: &str,
    agent: Option<&str>,
    status: &str,
    event: &str,
) {
    let now = now_rfc3339();
    let ep = now_epoch();
    let mut item = store
        .calls
        .get(call_id)
        .map(|c| c.clone())
        .unwrap_or_else(|| {
            serde_json::json!({
                "call_id": call_id, "dialed_number": "", "status": "RINGING",
                "agent_id": serde_json::Value::Null, "caller_ip": "",
                "created_at": now, "created_epoch": ep, "shard": "ALL",
                "claimed_at": serde_json::Value::Null, "engaged_at": serde_json::Value::Null,
                "ended_at": serde_json::Value::Null, "ivr_finished_at": serde_json::Value::Null,
            })
        });
    item["status"] = serde_json::Value::String(status.to_string());
    match status {
        "WAITING" => {
            if item.get("ivr_finished_at").map(|v| v.is_null()).unwrap_or(true) {
                item["ivr_finished_at"] = serde_json::Value::String(now.clone());
            }
        }
        "ENGAGED" => {
            if let Some(a) = agent {
                item["agent_id"] = serde_json::Value::String(a.to_string());
            }
            if item.get("claimed_at").map(|v| v.is_null()).unwrap_or(true) {
                item["claimed_at"] = serde_json::Value::String(now.clone());
            }
            item["engaged_at"] = serde_json::Value::String(now.clone());
        }
        "ENDED" => {
            item["ended_at"] = serde_json::Value::String(now.clone());
            if let Some(a) = agent {
                if item.get("agent_id").map(|v| v.is_null()).unwrap_or(true) {
                    item["agent_id"] = serde_json::Value::String(a.to_string());
                }
            }
        }
        _ => {}
    }
    let agent_s = item
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let dialed = item
        .get("dialed_number")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let created_at = item
        .get("created_at")
        .and_then(|v| v.as_str())
        .unwrap_or(&now)
        .to_string();
    let created_epoch = item
        .get("created_epoch")
        .and_then(|v| v.as_i64())
        .unwrap_or(ep);
    store.calls.insert(call_id.to_string(), item);
    let mut ddb = serde_json::json!({
        "call_id": s(call_id),
        "dialed_number": s(&dialed),
        "status": s(status),
        "created_at": s(&created_at),
        "created_epoch": num(created_epoch),
        "shard": s("ALL"),
    });
    if let Some(a) = agent_s.or_else(|| agent.map(String::from)) {
        ddb["agent_id"] = s(&a);
    }
    match status {
        "WAITING" => {
            ddb["ivr_finished_at"] = s(&now);
        }
        "ENGAGED" => {
            ddb["claimed_at"] = s(&now);
            ddb["engaged_at"] = s(&now);
        }
        "ENDED" => {
            ddb["ended_at"] = s(&now);
        }
        _ => {}
    }
    put_item("vecna_calls", ddb);
    log_event(store, call_id, agent, event, None).await;
}

pub async fn record_call_location(store: &Store, call_id: &str, lat: f64, lng: f64, accuracy: Option<f64>) {
    let loc = serde_json::json!({ "lat": lat, "lng": lng, "accuracy": accuracy });
    if let Some(mut c) = store.calls.get_mut(call_id) {
        c["location"] = loc;
    }
    // update-item (not put-item) so the call row isn't clobbered
    let mut m = serde_json::Map::new();
    m.insert("lat".into(), fnum(lat));
    m.insert("lng".into(), fnum(lng));
    if let Some(a) = accuracy {
        m.insert("accuracy".into(), fnum(a));
    }
    fire(vec![
        "dynamodb".into(),
        "update-item".into(),
        "--table-name".into(),
        "vecna_calls".into(),
        "--key".into(),
        serde_json::json!({ "call_id": s(call_id) }).to_string(),
        "--update-expression".into(),
        "SET location = :loc".into(),
        "--expression-attribute-values".into(),
        serde_json::json!({ ":loc": serde_json::json!({ "M": serde_json::Value::Object(m) }) }).to_string(),
        "--region".into(),
        REGION.into(),
    ]);
}

pub async fn record_recording(
    store: &Store,
    call_id: &str,
    side: &str,
    filename: &str,
    bytes: usize,
    mime: &str,
    rms: Option<f64>,
    zcr: Option<f64>,
) -> Option<String> {
    record_recording_s3(store, call_id, side, filename, bytes, mime, rms, zcr, "s3", None).await
}

pub async fn record_recording_s3(
    store: &Store,
    call_id: &str,
    side: &str,
    filename: &str,
    bytes: usize,
    mime: &str,
    rms: Option<f64>,
    zcr: Option<f64>,
    storage: &str,
    s3_key: Option<&str>,
) -> Option<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_rfc3339();
    let ep = now_epoch();
    let key = s3_key
        .map(String::from)
        .unwrap_or_else(|| format!("calls/{call_id}/{filename}"));
    store.recordings.insert(
        id.clone(),
        serde_json::json!({
            "recording_id": id, "call_id": call_id, "side": side,
            "filename": filename, "bytes": bytes as i64, "mime": mime,
            "rms": rms, "zcr": zcr, "storage": storage, "s3_key": key,
            "created_at": now, "created_epoch": ep,
        }),
    );
    let mut ddb = serde_json::json!({
        "recording_id": s(&id),
        "call_id": s(call_id),
        "created_epoch": num(ep),
        "side": s(side),
        "filename": s(filename),
        "bytes": num(bytes as i64),
        "mime": s(mime),
        "storage": s(storage),
        "s3_key": s(&key),
        "created_at": s(&now),
    });
    if let Some(r) = rms {
        ddb["rms"] = fnum(r);
    }
    if let Some(z) = zcr {
        ddb["zcr"] = fnum(z);
    }
    put_item("vecna_recordings", ddb);
    log_event(
        store,
        call_id,
        None,
        "RECORDING_UPLOADED",
        Some(&serde_json::json!({ "recording_id": id, "side": side, "bytes": bytes, "storage": storage, "s3_key": key })),
    )
    .await;
    Some(id)
}

fn call_of_recording(store: &Store, recording_id: &str) -> Option<String> {
    store
        .recordings
        .get(recording_id)
        .and_then(|r| r.get("call_id").and_then(|c| c.as_str()).map(String::from))
}

pub async fn record_transcript(
    store: &Store,
    recording_id: &str,
    language: Option<&str>,
    text: &str,
    confidence: Option<f64>,
    provider: &str,
) -> Option<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_rfc3339();
    let ep = now_epoch();
    let call_id = call_of_recording(store, recording_id).unwrap_or_default();
    store.transcripts.insert(
        id.clone(),
        serde_json::json!({
            "transcript_id": id, "recording_id": recording_id, "call_id": call_id,
            "language": language.unwrap_or_default(), "text": text,
            "confidence": confidence, "provider": provider,
            "created_at": now, "created_epoch": ep,
        }),
    );
    let mut ddb = serde_json::json!({
        "transcript_id": s(&id),
        "call_id": s(&call_id),
        "created_epoch": num(ep),
        "recording_id": s(recording_id),
        "language": s(language.unwrap_or_default()),
        "text": s(text),
        "provider": s(provider),
        "created_at": s(&now),
    });
    if let Some(c) = confidence {
        ddb["confidence"] = fnum(c);
    }
    put_item("vecna_transcripts", ddb);
    Some(id)
}

pub async fn record_emotion(
    store: &Store,
    call_id: &str,
    label: &str,
    confidence: f64,
    scores: &serde_json::Value,
    urgency: i64,
    source: &str,
    recording_id: Option<&str>,
) {
    record_emotion_persist(store, call_id, label, confidence, scores, urgency, source, recording_id, source != "live").await
}

pub async fn record_emotion_persist(
    store: &Store,
    call_id: &str,
    label: &str,
    confidence: f64,
    scores: &serde_json::Value,
    urgency: i64,
    source: &str,
    recording_id: Option<&str>,
    persist: bool,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_rfc3339();
    let ep = now_epoch();
    let row = serde_json::json!({
        "emotion_id": id, "id": id, "call_id": call_id, "label": label,
        "confidence": confidence, "scores_json": scores.to_string(),
        "scores": scores, "urgency": urgency, "source": source,
        "recording_id": recording_id,
        "created_at": now, "created_epoch": ep,
    });
    store
        .emotions
        .entry(call_id.to_string())
        .or_default()
        .push(row);
    if !persist {
        // pre-claim live readings: memory-only queue chip, no DB row
        return;
    }
    let mut ddb = serde_json::json!({
        "emotion_id": s(&id),
        "call_id": s(call_id),
        "created_epoch": num(ep),
        "label": s(label),
        "confidence": fnum(confidence),
        "scores_json": s(&scores.to_string()),
        "urgency": num(urgency),
        "source": s(source),
        "created_at": s(&now),
    });
    if let Some(r) = recording_id {
        ddb["recording_id"] = s(r);
    }
    put_item("vecna_emotions", ddb);
}

pub async fn record_analysis(
    store: &Store,
    recording_id: &str,
    kind: &str,
    provider: &str,
    model: Option<&str>,
    result_json: &serde_json::Value,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_rfc3339();
    let ep = now_epoch();
    let call_id = call_of_recording(store, recording_id).unwrap_or_default();
    store.analyses.insert(
        id.clone(),
        serde_json::json!({
            "analysis_id": id, "recording_id": recording_id, "call_id": call_id,
            "kind": kind, "provider": provider, "model": model.unwrap_or_default(),
            "result_json": result_json.to_string(),
            "created_at": now, "created_epoch": ep,
        }),
    );
    put_item(
        "vecna_analyses",
        serde_json::json!({
            "analysis_id": s(&id),
            "call_id": s(&call_id),
            "created_epoch": num(ep),
            "recording_id": s(recording_id),
            "kind": s(kind),
            "provider": s(provider),
            "model": s(model.unwrap_or_default()),
            "result_json": s(&result_json.to_string()),
            "created_at": s(&now),
        }),
    );
}

pub async fn log_event(
    store: &Store,
    call_id: &str,
    agent_id: Option<&str>,
    event: &str,
    detail: Option<&serde_json::Value>,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_rfc3339();
    let ep = now_epoch();
    let row = serde_json::json!({
        "event_id": id, "call_id": call_id, "agent_id": agent_id,
        "event": event, "detail_json": detail.map(|d| d.to_string()),
        "created_at": now,
    });
    store.events.entry(call_id.to_string()).or_default().push(row);
    put_item(
        "vecna_events",
        serde_json::json!({
            "event_id": s(&id),
            "call_id": s(call_id),
            "created_epoch": num(ep),
            "agent_id": sn(agent_id),
            "event": s(event),
            "detail_json": sn(detail.map(|d| d.to_string()).as_deref()),
            "created_at": s(&now),
        }),
    );
}

pub async fn touch_agent_login(store: &Store, agent_id: &str) {
    let now = now_rfc3339();
    if let Some(mut a) = store.agents.get_mut(agent_id) {
        a["last_login_at"] = serde_json::Value::String(now.clone());
    }
    fire(vec![
        "dynamodb".into(),
        "update-item".into(),
        "--table-name".into(),
        "vecna_agents".into(),
        "--key".into(),
        serde_json::json!({ "agent_id": s(agent_id) }).to_string(),
        "--update-expression".into(),
        "SET last_login_at = :t".into(),
        "--expression-attribute-values".into(),
        serde_json::json!({ ":t": s(&now) }).to_string(),
        "--region".into(),
        REGION.into(),
    ]);
}
