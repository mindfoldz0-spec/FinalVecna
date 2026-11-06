// Shared signaling protocol: backend <-> browser (JSON over WebSocket)

export type CallerOut =
  | { action: 'PING' }
  | { action: 'IVR_FINISHED' }
  | { action: 'WEBRTC_SIGNAL'; call_id: string; payload: unknown }
  | { action: 'END_CALL'; call_id: string };

export type AgentOut =
  | { action: 'PING' }
  | { action: 'SYNC_QUEUE' }
  | { action: 'CLAIM_CALL'; call_id: string }
  | { action: 'RELEASE_CALL'; call_id: string }
  | { action: 'WEBRTC_SIGNAL'; call_id: string; payload: unknown }
  | { action: 'END_CALL'; call_id: string };

export interface EmotionInfo {
  label: string;
  confidence: number;
  urgency: number; // 0..3 distress weight
  scores: Record<string, number>;
  source: 'live' | 'final';
  at?: number;
}

/** LLM triage of the transcribed message (Qwen via Groq). */
export interface TriageInfo {
  category: string; // cybercrime-financial | image-based-abuse | physical-crime | ...
  label: string;    // short "what is happening" label
  urgency: number;  // 0..3
  summary: string;
  keywords: string[];
  flags: string[];  // csam | minor | immediate-danger | financial-loss | ongoing
  at?: number;
}

export interface CallInfo {
  call_id: string;
  dialed_number: string;
  status: 'RINGING' | 'WAITING' | 'ENGAGED' | 'ENDED';
  agent_id: string | null;
  created_at: number;
  engaged_at: number | null;
  ended_at: number | null;
  emotion?: EmotionInfo | null;
  triage?: TriageInfo | null;
}

export type ServerEvent =
  | { event: 'PONG' }
  | { event: 'QUEUE_SNAPSHOT'; calls: CallInfo[] }
  | { event: 'NEW_CALL'; call: CallInfo }
  | { event: 'STATUS_CHANGED'; call_id: string; status: string; call?: CallInfo }
  | { event: 'CALL_CLAIMED'; call_id: string; agent_id: string }
  | { event: 'CALL_RELEASED'; call_id: string }
  | { event: 'AGENT_ASSIGNED'; call_id: string; agent_id: string }
  | { event: 'CALL_ENDED'; call_id: string }
  | { event: 'WEBRTC_SIGNAL'; call_id?: string; payload: unknown }
  | { event: 'CLAIM_RESULT'; call_id: string; success: boolean; reason?: string; emotion?: EmotionInfo | null; triage?: TriageInfo | null }
  | { event: 'EMOTION_UPDATE'; call_id: string; label: string; confidence: number; urgency: number; scores: Record<string, number>; source: 'live' | 'final' }
  | { event: 'TRIAGE_UPDATE'; call_id: string; category: string; label: string; urgency: number; summary: string; keywords: string[]; flags: string[]; transcript?: string; stt_provider?: string; at?: number }
  | { event: 'DUPLICATE_LOGIN' }
  | { event: 'ERROR'; call_id?: string; message: string };
