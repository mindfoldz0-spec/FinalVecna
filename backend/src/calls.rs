use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::Serialize;
use uuid::Uuid;

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CallStatus {
    Ringing,
    Waiting,
    Engaged,
    Ended,
}

impl CallStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CallStatus::Ringing => "RINGING",
            CallStatus::Waiting => "WAITING",
            CallStatus::Engaged => "ENGAGED",
            CallStatus::Ended => "ENDED",
        }
    }

    pub fn claimable(&self) -> bool {
        matches!(self, CallStatus::Ringing | CallStatus::Waiting)
    }
}

/// Latest emotion reading for a call (live-streamed or final on the recording).
#[derive(Debug, Clone, Serialize)]
pub struct EmotionView {
    pub label: String,
    pub confidence: f64,
    pub urgency: i64,
    pub scores: serde_json::Value,
    pub source: String,
    pub at: f64,
}

/// Triage of the transcribed message (Qwen via Groq): what is happening and
/// how urgent — shown in the officer queue as a category label + urgency panel.
#[derive(Debug, Clone, Serialize)]
pub struct TriageView {
    pub category: String,
    pub label: String,
    pub urgency: i64,
    pub summary: String,
    pub keywords: Vec<String>,
    pub flags: Vec<String>,
    pub provider: String,
    pub at: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Call {
    pub call_id: String,
    pub dialed_number: String,
    pub status: CallStatus,
    pub agent_id: Option<String>,
    pub created_at: f64,
    pub engaged_at: Option<f64>,
    pub ended_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emotion: Option<EmotionView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triage: Option<TriageView>,
}

impl Call {
    pub fn new(dialed_number: String) -> Self {
        Self {
            call_id: Uuid::new_v4().to_string(),
            dialed_number,
            status: CallStatus::Ringing,
            agent_id: None,
            created_at: now_secs(),
            engaged_at: None,
            ended_at: None,
            emotion: None,
            triage: None,
        }
    }
}

/// In-memory call registry. DashMap gives lock-free per-key access;
/// claim/release/end use entry-level atomicity so two agents can
/// never grab the same call — no global lock, no single-worker hack.
pub struct CallManager {
    calls: DashMap<String, Call>,
    /// call_id -> caller socket sender
    pub caller_sockets: DashMap<String, tokio::sync::mpsc::UnboundedSender<String>>,
    /// call_id -> caller connection generation (bumped on every (re)connect)
    caller_gen: DashMap<String, u64>,
    /// agent_id -> agent socket sender (Arc so a disconnecting task can verify
    /// it still owns the slot before cleaning up)
    pub agent_sockets: DashMap<String, std::sync::Arc<tokio::sync::mpsc::UnboundedSender<String>>>,
    /// agent_id -> call_id they are engaged on (atomic one-call-per-agent slot)
    agent_active: DashMap<String, String>,
    /// serializes claim/release/end so the state transitions and the
    /// agent_active bookkeeping are atomic w.r.t. each other
    state_lock: Mutex<()>,
}

impl CallManager {
    pub fn new() -> Self {
        Self {
            calls: DashMap::new(),
            caller_sockets: DashMap::new(),
            caller_gen: DashMap::new(),
            agent_sockets: DashMap::new(),
            agent_active: DashMap::new(),
            state_lock: Mutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.state_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a caller socket; returns the generation of this connection.
    /// A later registration with the same call_id bumps the generation, which
    /// tells the disconnect-grace task that the caller reconnected.
    pub fn register_caller(
        &self,
        call_id: &str,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> u64 {
        let mut e = self.caller_gen.entry(call_id.to_string()).or_insert(0);
        *e += 1;
        let gen = *e;
        drop(e);
        self.caller_sockets.insert(call_id.to_string(), tx);
        gen
    }

    pub fn caller_gen(&self, call_id: &str) -> u64 {
        self.caller_gen.get(call_id).map(|g| *g).unwrap_or(0)
    }

    pub fn create(&self, dialed_number: String) -> Call {
        let call = Call::new(dialed_number);
        self.calls.insert(call.call_id.clone(), call.clone());
        call
    }

    pub fn get(&self, call_id: &str) -> Option<Call> {
        self.calls.get(call_id).map(|c| c.clone())
    }

    pub fn set_emotion(&self, call_id: &str, emo: EmotionView) {
        if let Some(mut c) = self.calls.get_mut(call_id) {
            c.emotion = Some(emo);
        }
    }

    pub fn set_triage(&self, call_id: &str, t: TriageView) {
        if let Some(mut c) = self.calls.get_mut(call_id) {
            c.triage = Some(t);
        }
    }

    pub fn snapshot(&self) -> Vec<Call> {
        let mut v: Vec<Call> = self
            .calls
            .iter()
            .filter(|c| !matches!(c.status, CallStatus::Ended))
            .map(|c| c.clone())
            .collect();
        v.sort_by(|a, b| a.created_at.partial_cmp(&b.created_at).unwrap());
        v
    }

    /// RINGING -> WAITING. Returns true only when the transition happened,
    /// so callers don't broadcast on a duplicate/late IVR_FINISHED.
    pub fn set_waiting(&self, call_id: &str) -> bool {
        if let Some(mut c) = self.calls.get_mut(call_id) {
            if matches!(c.status, CallStatus::Ringing) {
                c.status = CallStatus::Waiting;
                return true;
            }
        }
        false
    }

    /// Atomically claim a call. Only WAITING calls (caller finished the IVR
    /// message) can be claimed — RINGING means the caller is still recording.
    /// The agent's one-active-call slot is reserved atomically under state_lock,
    /// so two agents can never double-book.
    pub fn claim(&self, call_id: &str, agent_id: &str) -> Result<Call, &'static str> {
        let _g = self.lock();
        if self.agent_active.contains_key(agent_id) {
            return Err("already on another call");
        }
        let mut c = self.calls.get_mut(call_id).ok_or("call not found")?;
        match c.status {
            CallStatus::Waiting => {
                c.status = CallStatus::Engaged;
                c.agent_id = Some(agent_id.to_string());
                c.engaged_at = Some(now_secs());
                let call = c.clone();
                drop(c);
                self.agent_active.insert(agent_id.to_string(), call_id.to_string());
                Ok(call)
            }
            CallStatus::Ringing => Err("caller is still recording their message"),
            CallStatus::Engaged => Err("already claimed"),
            CallStatus::Ended => Err("call ended"),
        }
    }

    pub fn release(&self, call_id: &str, agent_id: &str) -> Result<Call, &'static str> {
        let _g = self.lock();
        let mut c = self.calls.get_mut(call_id).ok_or("call not found")?;
        if c.agent_id.as_deref() != Some(agent_id) {
            return Err("not your call");
        }
        // Only an ENGAGED call can be released — a late RELEASE_CALL must not
        // resurrect an ENDED call back into the queue.
        if !matches!(c.status, CallStatus::Engaged) {
            return Err("call is not engaged");
        }
        c.status = CallStatus::Waiting;
        c.agent_id = None;
        c.engaged_at = None;
        let call = c.clone();
        drop(c);
        self.agent_active.remove(agent_id);
        Ok(call)
    }

    pub fn end(&self, call_id: &str, agent_id: Option<&str>) -> Result<Call, &'static str> {
        let _g = self.lock();
        let mut c = self.calls.get_mut(call_id).ok_or("call not found")?;
        if let Some(a) = agent_id {
            if c.agent_id.as_deref() != Some(a) {
                return Err("not your call");
            }
        }
        if matches!(c.status, CallStatus::Ended) {
            return Ok(c.clone()); // idempotent
        }
        c.status = CallStatus::Ended;
        c.ended_at = Some(now_secs());
        let owner = c.agent_id.clone();
        let call = c.clone();
        drop(c);
        if let Some(a) = owner {
            self.agent_active.remove_if(&a, |_, v| v == call_id);
        }
        Ok(call)
    }

    pub fn purge_ended(&self, ttl_secs: f64) -> Vec<String> {
        let now = now_secs();
        let dead: Vec<String> = self
            .calls
            .iter()
            .filter(|c| {
                matches!(c.status, CallStatus::Ended)
                    && c.ended_at.map(|e| now - e > ttl_secs).unwrap_or(false)
            })
            .map(|c| c.call_id.clone())
            .collect();
        for id in &dead {
            self.calls.remove(id);
        }
        dead
    }

    /// Backstop: remove non-ended calls that have been sitting with no live
    /// caller socket for longer than max_age_secs (caller vanished without the
    /// grace task firing, server restarted mid-call, etc).
    pub fn purge_stale(&self, max_age_secs: f64) -> Vec<String> {
        let now = now_secs();
        let stale: Vec<String> = self
            .calls
            .iter()
            .filter(|c| {
                !matches!(c.status, CallStatus::Ended)
                    && now - c.created_at > max_age_secs
                    && !self.caller_sockets.contains_key(c.key())
            })
            .map(|c| c.call_id.clone())
            .collect();
        for id in &stale {
            if let Some((_, call)) = self.calls.remove(id) {
                if let Some(a) = call.agent_id {
                    self.agent_active.remove_if(&a, |_, v| v == id);
                }
            }
            self.caller_gen.remove(id);
        }
        stale
    }
}

impl Default for CallManager {
    fn default() -> Self {
        Self::new()
    }
}
