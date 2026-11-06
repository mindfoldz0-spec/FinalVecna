import { useEffect, useRef, useState } from 'react';
import CrystalizedBall from '../components/CrystalizedBall';
import Strands from '../components/Strands';
import '../components/CrystalizedBall.css';
import '../components/Strands.css';
import './agent.css';
import type { CallInfo, EmotionInfo, ServerEvent, TriageInfo } from '../lib/protocol';
import { CallSocket, wsUrl } from '../lib/ws';
import { applyRemote, createPeer } from '../lib/rtc';
import { startCallRecorder, stopAndUpload } from '../lib/recorder';

const FALLBACK_AGENTS = ['mayuresh_super', 'tejas_dimitri', 'Aman_clasp'];

type Tab = 'login' | 'console';

function EmotionChip({ emo }: { emo: EmotionInfo }) {
  const title = Object.entries(emo.scores)
    .sort((a, b) => b[1] - a[1])
    .map(([k, v]) => `${k} ${(v * 100).toFixed(0)}%`)
    .join('\n');
  return (
    <span className={`emo u${emo.urgency}`} title={title}>
      {emo.label} · {Math.round(emo.confidence * 100)}%
    </span>
  );
}

/** Triage panel: LLM category label + urgency + one-line summary of the
 *  transcribed message. Flags (csam, immediate-danger, ...) get badges. */
function TriagePanel({ t }: { t: TriageInfo }) {
  return (
    <div className={`triage u${t.urgency}`}>
      <div className="tri-row">
        <span className="cat">{t.category}</span>
        <span className="tri-urg">URGENCY {t.urgency}/3</span>
      </div>
      <div className="tri-label">{t.label}</div>
      {t.summary && <div className="tri-sum">{t.summary}</div>}
      {t.flags.length > 0 && (
        <div className="tri-flags">{t.flags.map((f) => <span key={f} className="flag">{f}</span>)}</div>
      )}
    </div>
  );
}

/** Effective urgency of a queued call = worse of emotion and text triage. */
function effectiveUrgency(c: CallInfo, emotions: Record<string, EmotionInfo>, triages: Record<string, TriageInfo>): number {
  return Math.max(emotions[c.call_id]?.urgency ?? -1, triages[c.call_id]?.urgency ?? -1);
}

export default function Agent() {
  const [tab, setTab] = useState<Tab>('login');
  const [agents, setAgents] = useState<string[]>(FALLBACK_AGENTS);
  const [agentId, setAgentId] = useState(FALLBACK_AGENTS[0]);
  const [password, setPassword] = useState('');
  const [loginError, setLoginError] = useState('');
  const [calls, setCalls] = useState<CallInfo[]>([]);
  // call_id -> latest emotion reading (live while queued, final after upload)
  const [emotions, setEmotions] = useState<Record<string, EmotionInfo>>({});
  // call_id -> latest triage of the transcribed message (arrives after upload)
  const [triages, setTriages] = useState<Record<string, TriageInfo>>({});
  const [activeCallId, setActiveCallId] = useState<string | null>(null);
  const [status, setStatus] = useState('online');
  const [muted, setMuted] = useState(false);

  const socketRef = useRef<CallSocket<unknown> | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const pcRef = useRef<RTCPeerConnection | null>(null);
  const remoteAudioRef = useRef<HTMLAudioElement | null>(null);
  const bufferedRef = useRef<RTCIceCandidateInit[]>([]);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const callerCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const agentCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const rafRef = useRef<number | null>(null);
  // the waves visualisation context — closed on hangup (browsers cap ~6 running)
  const waveCtxRef = useRef<AudioContext | null>(null);
  // Mirrors activeCallId state for the WebSocket handler, which is registered
  // once at connect time and would otherwise close over a stale `null`.
  const activeCallIdRef = useRef<string | null>(null);
  // Signals that arrive before the peer connection exists (offer usually beats
  // getUserMedia) are queued here and flushed once the peer is ready.
  const pendingSignalsRef = useRef<{ call_id: string; payload: unknown }[]>([]);

  function setActiveCall(callId: string | null) {
    activeCallIdRef.current = callId;
    setActiveCallId(callId);
  }

  useEffect(() => {
    fetch('/api/agent/me', { credentials: 'same-origin' }).then((r) => {
      if (r.ok) r.json().then((m) => { setAgentId(m.agent_id); setTab('console'); });
    }).catch(() => {});
    // agent list comes from the backend so the two never drift apart
    fetch('/api/agent/list').then((r) => (r.ok ? r.json() : null))
      .then((j) => { if (Array.isArray(j?.agents) && j.agents.length) setAgents(j.agents); })
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (tab === 'console') connect();
    return () => { socketRef.current?.close(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  async function login(e: React.FormEvent) {
    e.preventDefault();
    setLoginError('');
    let res: Response;
    try {
      res = await fetch('/api/agent/login', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ agent_id: agentId, password }),
      });
    } catch {
      setLoginError('Network error — is the server running?');
      return;
    }
    if (res.ok) setTab('console');
    else setLoginError('Invalid officer or password');
  }

  function connect() {
    const socket = new CallSocket(wsUrl(`/ws/agent/${encodeURIComponent(agentId)}`), {
      autoReconnect: true,
      onEvent: (msg) => void handleEvent(msg),
      // undo the "reconnecting…" label once the socket is back
      onOpen: () => setStatus(activeCallIdRef.current ? 'in call' : 'online'),
      onClose: () => setStatus('reconnecting…'),
    });
    socketRef.current = socket as CallSocket<unknown>;
    socket.connect();
  }

  // merge a call into the list and seed its emotion/triage chips if present
  function mergeCall(call: CallInfo) {
    setCalls((prev) => [...prev.filter((c) => c.call_id !== call.call_id), call]);
    if (call.emotion) {
      setEmotions((prev) => (prev[call.call_id] ? prev : { ...prev, [call.call_id]: call.emotion! }));
    }
    if (call.triage) {
      setTriages((prev) => (prev[call.call_id] ? prev : { ...prev, [call.call_id]: call.triage! }));
    }
  }

  async function handleEvent(msg: ServerEvent) {
    switch (msg.event) {
      case 'QUEUE_SNAPSHOT':
        setCalls(msg.calls);
        for (const c of msg.calls) {
          if (c.emotion) setEmotions((prev) => ({ ...prev, [c.call_id]: c.emotion! }));
          if (c.triage) setTriages((prev) => ({ ...prev, [c.call_id]: c.triage! }));
        }
        break;
      case 'NEW_CALL':
        mergeCall(msg.call);
        break;
      case 'EMOTION_UPDATE':
        setEmotions((prev) => ({
          ...prev,
          [msg.call_id]: {
            label: msg.label,
            confidence: msg.confidence,
            urgency: msg.urgency,
            scores: msg.scores,
            source: msg.source,
          },
        }));
        break;
      case 'TRIAGE_UPDATE':
        setTriages((prev) => ({
          ...prev,
          [msg.call_id]: {
            category: msg.category,
            label: msg.label,
            urgency: msg.urgency,
            summary: msg.summary,
            keywords: msg.keywords,
            flags: msg.flags,
            at: msg.at,
          },
        }));
        break;
      case 'STATUS_CHANGED': {
        if (msg.call) {
          mergeCall(msg.call);
        } else {
          const id = msg.call_id;
          const st = msg.status;
          setCalls((prev) => prev.map((c) => c.call_id === id ? { ...c, status: st as CallInfo['status'] } : c));
        }
        break;
      }
      case 'CALL_CLAIMED':
        setCalls((prev) => prev.map((c) => c.call_id === msg.call_id ? { ...c, status: 'ENGAGED', agent_id: msg.agent_id } : c));
        break;
      case 'CALL_RELEASED':
        setCalls((prev) => prev.map((c) => c.call_id === msg.call_id ? { ...c, status: 'WAITING', agent_id: null } : c));
        if (activeCallIdRef.current === msg.call_id) void hangup(false);
        break;
      case 'CALL_ENDED':
        setCalls((prev) => prev.filter((c) => c.call_id !== msg.call_id));
        setEmotions((prev) => {
          const next = { ...prev };
          delete next[msg.call_id];
          return next;
        });
        setTriages((prev) => {
          const next = { ...prev };
          delete next[msg.call_id];
          return next;
        });
        if (activeCallIdRef.current === msg.call_id) void hangup(false);
        break;
      case 'CLAIM_RESULT':
        if (msg.success) void startCallSession(msg.call_id);
        else setStatus(`cannot claim: ${msg.reason ?? ''}`);
        break;
      case 'WEBRTC_SIGNAL': {
        const sigCallId = msg.call_id ?? activeCallIdRef.current;
        if (!sigCallId) break;
        if (!pcRef.current || sigCallId !== activeCallIdRef.current) {
          // Peer not ready yet (offer usually arrives before getUserMedia
          // resolves) — buffer it and flush once the peer exists.
          pendingSignalsRef.current.push({ call_id: sigCallId, payload: msg.payload });
          break;
        }
        try {
          const payload = msg.payload as { offer?: RTCSessionDescriptionInit; answer?: RTCSessionDescriptionInit; candidate?: RTCIceCandidateInit };
          const out = await applyRemote(pcRef.current, payload, bufferedRef.current);
          if (out.answer) socketRef.current?.send({ action: 'WEBRTC_SIGNAL', call_id: sigCallId, payload: { answer: out.answer } });
        } catch (e) {
          console.error('[agent] failed to apply remote signal', e);
        }
        break;
      }
      case 'DUPLICATE_LOGIN':
        setStatus('logged in elsewhere');
        break;
      default:
        break;
    }
  }

  async function claim(callId: string) {
    // unlock the audio pipeline while the click's user activation is fresh
    const unlock = new Audio('/audio/unlock.wav');
    unlock.play().catch(() => {});
    socketRef.current?.send({ action: 'CLAIM_CALL', call_id: callId });
  }

  function release() {
    const callId = activeCallIdRef.current;
    if (callId) socketRef.current?.send({ action: 'RELEASE_CALL', call_id: callId });
    // CALL_RELEASED comes back from the server and runs hangup(false)
  }

  async function startCallSession(callId: string) {
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch {
      setStatus('mic required — call returned to queue');
      socketRef.current?.send({ action: 'RELEASE_CALL', call_id: callId });
      return;
    }
    streamRef.current = stream;
    setActiveCall(callId);
    setStatus('in call');

    const socket = socketRef.current;
    const pc = await createPeer({
      localStream: stream,
      onRemoteStream: (remote) => {
        const el = remoteAudioRef.current;
        if (el) {
          el.srcObject = remote;
          el.volume = 1;
          el.muted = false;
          const p = el.play();
          if (p) p.catch((e: DOMException) => {
            if (e?.name === 'NotAllowedError') {
              // ontrack fires after async setup — resume on next tap
              const retry = () => {
                window.removeEventListener('pointerdown', retry);
                el.play().catch(() => {});
              };
              window.addEventListener('pointerdown', retry);
              setStatus('Tap anywhere to enable audio');
            }
          });
        }
        startWaves(remote);
        recorderRef.current = startCallRecorder(stream);
      },
      onSignal: (payload) => socket?.send({ action: 'WEBRTC_SIGNAL', call_id: callId, payload }),
      onStateChange: (s) => {
        if (s === 'failed' || s === 'disconnected') setStatus('Connection lost…');
      },
    });
    pcRef.current = pc;
    // Agent ANSWERS: flush anything that arrived before the peer existed
    // (usually the caller's offer).
    try {
      for (const q of pendingSignalsRef.current.splice(0)) {
        if (q.call_id !== callId) continue;
        const out = await applyRemote(pc, q.payload as { offer?: RTCSessionDescriptionInit; answer?: RTCSessionDescriptionInit; candidate?: RTCIceCandidateInit }, bufferedRef.current);
        if (out.answer) socket?.send({ action: 'WEBRTC_SIGNAL', call_id: callId, payload: { answer: out.answer } });
      }
    } catch (e) {
      console.error('[agent] failed to flush buffered signals', e);
    }
  }

  function startWaves(remote: MediaStream) {
    const ctx = new AudioContext();
    waveCtxRef.current = ctx;
    const agentSrc = ctx.createMediaStreamSource(streamRef.current!);
    const callerSrc = ctx.createMediaStreamSource(remote);
    const a1 = ctx.createAnalyser(); a1.fftSize = 256;
    const a2 = ctx.createAnalyser(); a2.fftSize = 256;
    agentSrc.connect(a1);
    callerSrc.connect(a2);
    const buf = new Uint8Array(256);
    const draw = (canvas: HTMLCanvasElement | null, analyser: AnalyserNode, color: string) => {
      if (!canvas) return;
      const g = canvas.getContext('2d')!;
      analyser.getByteTimeDomainData(buf);
      // opaque fill — a translucent one compounds every frame and washes
      // the canvas to light blue, hiding the (light) waveform.
      g.fillStyle = '#02243d';
      g.fillRect(0, 0, canvas.width, canvas.height);
      const trace = (width: number, alpha: number) => {
        g.strokeStyle = color;
        g.globalAlpha = alpha;
        g.lineWidth = width;
        g.lineJoin = 'round';
        g.beginPath();
        for (let i = 0; i < buf.length; i++) {
          const x = (i / buf.length) * canvas.width;
          const y = (buf[i] / 255) * canvas.height;
          i === 0 ? g.moveTo(x, y) : g.lineTo(x, y);
        }
        g.stroke();
      };
      trace(7, 0.18); // glow
      trace(2.5, 1); // core
      g.globalAlpha = 1;
    };
    const loop = () => {
      draw(callerCanvasRef.current, a2, '#ffb703');
      draw(agentCanvasRef.current, a1, '#ade8f4');
      rafRef.current = requestAnimationFrame(loop);
    };
    loop();
  }

  async function hangup(notify = true) {
    const callId = activeCallIdRef.current;
    const rec = recorderRef.current;
    recorderRef.current = null;
    if (notify && callId) socketRef.current?.send({ action: 'END_CALL', call_id: callId });
    try { pcRef.current?.close(); } catch { /* ignore */ }
    pcRef.current = null;
    bufferedRef.current = [];
    streamRef.current?.getTracks().forEach((t) => t.stop());
    streamRef.current = null;
    if (rafRef.current) cancelAnimationFrame(rafRef.current);
    waveCtxRef.current?.close().catch(() => {});
    waveCtxRef.current = null;
    if (callId && rec) await stopAndUpload(rec, callId, '', 'agent_audio');
    pendingSignalsRef.current = pendingSignalsRef.current.filter((q) => q.call_id !== callId);
    setActiveCall(null);
    setStatus('online');
  }

  function toggleMute() {
    const s = streamRef.current;
    if (!s) return;
    const next = !muted;
    s.getAudioTracks().forEach((t) => { t.enabled = !next; });
    setMuted(next);
  }

  // most urgent caller first — effective urgency = max(emotion, triage) —
  // oldest first within a level
  const sortedCalls = [...calls].sort((a, b) => {
    const ua = effectiveUrgency(a, emotions, triages);
    const ub = effectiveUrgency(b, emotions, triages);
    return ub - ua || a.created_at - b.created_at;
  });

  if (tab === 'login') {
    return (
      <div className="agent-login">
        <form className="login-card" onSubmit={login}>
          <h1>Officer Sign In</h1>
          <select value={agentId} onChange={(e) => setAgentId(e.target.value)}>
            {agents.map((a) => <option key={a} value={a}>{a}</option>)}
          </select>
          <input
            type="password"
            placeholder="Password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
          <button type="submit">Sign in</button>
          {loginError && <p className="login-error">{loginError}</p>}
        </form>
      </div>
    );
  }

  return (
    <div className="agent-console">
      <aside className="queue">
        <div className="queue-head">
          <span>{agentId}</span>
          <span className={`dot ${status === 'online' ? 'ok' : 'bad'}`} />{status}
        </div>
        <h2>Incoming ({calls.length})</h2>
        {calls.length === 0 && <p className="empty">No calls waiting</p>}
        {sortedCalls.map((c) => {
          const urg = effectiveUrgency(c, emotions, triages);
          return (
            <div key={c.call_id} className={`queue-item st-${c.status.toLowerCase()}`}>
              <div className="q-num">
                {c.dialed_number}
                {urg >= 0 && <span className={`urg u${urg}`}>U{urg}</span>}
              </div>
              <div className="q-meta">
                {c.status}{c.agent_id ? ` · ${c.agent_id}` : ''}
                {emotions[c.call_id] && <EmotionChip emo={emotions[c.call_id]} />}
              </div>
              {triages[c.call_id] && <TriagePanel t={triages[c.call_id]} />}
              {c.status === 'WAITING' && (
                <button onClick={() => claim(c.call_id)} disabled={!!activeCallId}>Accept</button>
              )}
            </div>
          );
        })}
        {/* Orb lives in the queue panel: always visible while nothing is
            queued and no call is active; replaced by the call list otherwise. */}
        {calls.length === 0 && !activeCallId && (
          <div className="queue-orb"><Strands colors={['#02243d', '#013a63', '#0a4a73', '#1273a8', '#5ab9d6', '#7fd1e8', '#ade8f4']} /></div>
        )}
      </aside>

      <main className="active">
        {!activeCallId ? (
          <div className="idle"><p>Pick a call from the queue</p></div>
        ) : (
          <div className="incall">
            <div className="incall-orb">
              <CrystalizedBall preset="abyss" particleCount={6000} size={0.55} speed={1} />
            </div>
            {activeCallId && emotions[activeCallId] && (
              <div className="incall-emo">Caller emotion: <EmotionChip emo={emotions[activeCallId]} /></div>
            )}
            {activeCallId && triages[activeCallId] && (
              <div className="incall-triage"><TriagePanel t={triages[activeCallId]} /></div>
            )}
            <div className="waves">
              <div className="wave">
                <span className="wave-label caller">Caller</span>
                <canvas ref={callerCanvasRef} width={360} height={70} />
              </div>
              <div className="wave">
                <span className="wave-label agent">You</span>
                <canvas ref={agentCanvasRef} width={360} height={70} />
              </div>
            </div>
            <div className="incall-btns">
              <button onClick={toggleMute} className={muted ? 'on' : ''}>{muted ? 'Unmute' : 'Mute'}</button>
              <button onClick={() => release()}>Release</button>
              <button className="danger" onClick={() => void hangup(true)}>End call</button>
            </div>
          </div>
        )}
        <audio ref={remoteAudioRef} autoPlay playsInline style={{ display: 'none' }} />
      </main>
    </div>
  );
}
