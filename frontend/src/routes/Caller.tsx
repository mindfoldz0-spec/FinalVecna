import { useCallback, useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import CrystalizedBall from '../components/CrystalizedBall';
import '../components/CrystalizedBall.css';
import './caller.css';
import type { ServerEvent } from '../lib/protocol';
import { CallSocket, wsUrl } from '../lib/ws';
import { applyRemote, createPeer } from '../lib/rtc';
import { startCallRecorder, stopAndUpload } from '../lib/recorder';
import { playFirst, playOnce, playLoopFirst, startSilenceWatcher, stopAudio } from '../lib/ivr';
import { pcmToWav, startPcmStream } from '../lib/pcmStream';

type Phase = 'dialer' | 'greeting' | 'recording' | 'waiting' | 'incall';

const DIALED_DEFAULT = '2007';
const KEYS = ['1','2','3','4','5','6','7','8','9','*','0','#'];
const KEY_LETTERS: Record<string, string> = {
  '2': 'ABC', '3': 'DEF', '4': 'GHI', '5': 'JKL',
  '6': 'MNO', '7': 'PQRS', '8': 'TUV', '9': 'WXYZ', '0': '+',
};

const SILENCE_MS = 2200;      // pause this long => message is done
const MIN_MSG_MS = 3000;      // message always records at least this long
const MESSAGE_MAX_MS = 60000; // safety cap only; silence ends it way earlier

const PHASE_TEXT: Record<Exclude<Phase, 'dialer'>, string> = {
  greeting: '“Welcome… speak your message after the beep.”',
  recording: '● Recording your message — speak now',
  waiting: '“All officers are busy — please hold…”',
  incall: '“You are connected. An officer will assist you.”',
};

export default function Caller() {
  const [phase, setPhase] = useState<Phase>('dialer');
  const [dialed, setDialed] = useState(DIALED_DEFAULT);
  const [status, setStatus] = useState('Tap call to reach support');
  const [timer, setTimer] = useState('00:00');
  const [muted, setMuted] = useState(false);
  const [level, setLevel] = useState(0);
  const [msgLeft, setMsgLeft] = useState(MESSAGE_MAX_MS / 1000);

  const socketRef = useRef<CallSocket<unknown> | null>(null);
  const callIdRef = useRef<string | null>(null);
  const tokenRef = useRef<string | null>(null);
  // Latest browser-geolocation fix, if the caller granted permission.
  const locationRef = useRef<{ lat: number; lng: number; accuracy: number | null } | null>(null);

  // Caller location getter — fires once on dialer mount so the fix usually
  // lands before the call is placed. If it arrives mid-call, it is POSTed
  // straight to the backend and shown in the officer queue.
  useEffect(() => {
    if (!('geolocation' in navigator)) return;
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        const loc = {
          lat: pos.coords.latitude,
          lng: pos.coords.longitude,
          accuracy: pos.coords.accuracy,
        };
        locationRef.current = loc;
        const id = callIdRef.current;
        const tok = tokenRef.current;
        if (id && tok) {
          fetch(`/api/calls/${encodeURIComponent(id)}/location`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', 'x-call-token': tok },
            body: JSON.stringify(loc),
          }).catch(() => {});
        }
      },
      () => { /* denied / unavailable — proceed without a location */ },
      { enableHighAccuracy: false, timeout: 6000, maximumAge: 300000 },
    );
  }, []);
  const streamRef = useRef<MediaStream | null>(null);
  const pcRef = useRef<RTCPeerConnection | null>(null);
  const remoteAudioRef = useRef<HTMLAudioElement | null>(null);
  const bufferedRef = useRef<RTCIceCandidateInit[]>([]);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const secondsRef = useRef(0);
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const ivrAudioRef = useRef<HTMLAudioElement | null>(null);
  // every IVR element currently alive (ring, greeting, beep, confirmation, hold)
  const ivrElemsRef = useRef<Set<HTMLAudioElement>>(new Set());
  const watcherStopRef = useRef<(() => void) | null>(null);
  const msgTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const ivrRunningRef = useRef(false);
  // bumped on cleanup so a hung-up call's pending runIvr continuations
  // can never leak into the next call
  const ivrGenRef = useRef(0);
  const ivrDoneRef = useRef(false);
  // mirrors phase for handlers registered once at socket connect time
  const phaseRef = useRef<Phase>('dialer');
  // set the moment the user ends the call; finishMessage must not run after
  // this (stopWatcher would otherwise fire onDone into a dead call)
  const endingRef = useRef(false);
  // prevents a rapid double-tap on Call creating two calls
  const startingRef = useRef(false);
  // live PCM tap for emotion analysis (beep -> officer picks up)
  const pcmStopRef = useRef<(() => void) | null>(null);
  // PCM captured during the message window — uploaded as a WAV sidecar
  // (message_wav) so the Gnani STT fallback has a format it can read
  const pcmChunksRef = useRef<ArrayBuffer[]>([]);

  function stopPcm() {
    pcmStopRef.current?.();
    pcmStopRef.current = null;
  }

  function startPcm() {
    const stream = streamRef.current;
    if (!stream || pcmStopRef.current) return;
    pcmStopRef.current = startPcmStream(stream, (buf) => {
      socketRef.current?.sendBinary(buf);
      if (phaseRef.current === 'recording') pcmChunksRef.current.push(buf);
    });
  }

  /** Upload the message recording as webm + a WAV sidecar of the same audio. */
  async function uploadMessage(rec: MediaRecorder, callId: string, token: string) {
    const wav = pcmToWav(pcmChunksRef.current);
    pcmChunksRef.current = [];
    const extra: Array<[string, Blob, string]> =
      wav.size > 44 ? [['message_wav', wav, 'message_wav.wav']] : [];
    await stopAndUpload(rec, callId, token, 'message', extra);
  }

  const fmt = (s: number) =>
    `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`;

  useEffect(() => { phaseRef.current = phase; }, [phase]);

  const startTimer = useCallback(() => {
    secondsRef.current = 0;
    setTimer('00:00');
    if (timerRef.current) clearInterval(timerRef.current);
    timerRef.current = setInterval(() => {
      secondsRef.current += 1;
      setTimer(fmt(secondsRef.current));
    }, 1000);
  }, []);

  const stopTimer = useCallback(() => {
    if (timerRef.current) clearInterval(timerRef.current);
    timerRef.current = null;
  }, []);

  function trackIvr(el: HTMLAudioElement | null): HTMLAudioElement | null {
    if (el) ivrElemsRef.current.add(el);
    return el;
  }

  function stopIvrAudio() {
    ivrElemsRef.current.forEach((el) => stopAudio(el));
    ivrElemsRef.current.clear();
    ivrAudioRef.current = null;
  }

  function stopWatcher() {
    watcherStopRef.current?.();
    watcherStopRef.current = null;
    if (msgTimerRef.current) clearInterval(msgTimerRef.current);
    msgTimerRef.current = null;
    setLevel(0);
  }

  const cleanup = useCallback(() => {
    socketRef.current?.close();
    socketRef.current = null;
    stopPcm();
    try { pcRef.current?.close(); } catch { /* ignore */ }
    pcRef.current = null;
    bufferedRef.current = [];
    streamRef.current?.getTracks().forEach((t) => t.stop());
    streamRef.current = null;
    stopTimer();
    stopIvrAudio();
    stopWatcher();
    ivrGenRef.current += 1;
    ivrRunningRef.current = false;
    ivrDoneRef.current = false;
  }, [stopTimer]);

  useEffect(() => () => cleanup(), [cleanup]);

  async function startCall() {
    if (phase !== 'dialer' || startingRef.current) return;
    startingRef.current = true;
    endingRef.current = false;
    // Unlock the audio pipeline NOW, while the click's user activation is still
    // fresh — later play() calls (after async mic/fetch/WS) would otherwise be
    // blocked by the autoplay policy.
    const unlock = new Audio('/audio/unlock.wav');
    unlock.play().catch(() => {});
    const number = dialed.trim() || DIALED_DEFAULT;
    setPhase('greeting');
    setStatus('Connecting to support…');

    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: false });
    } catch {
      startingRef.current = false;
      setPhase('dialer');
      setStatus('Microphone blocked — allow access and try again');
      return;
    }
    streamRef.current = stream;

    let call_id: string, call_token: string;
    try {
      const res = await fetch('/api/calls', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ dialed_number: number, location: locationRef.current }),
      });
      if (!res.ok) throw new Error('gateway busy');
      ({ call_id, call_token } = await res.json());
    } catch {
      startingRef.current = false;
      setPhase('dialer');
      setStatus('Could not reach support — try again');
      stream.getTracks().forEach((t) => t.stop());
      return;
    }
    callIdRef.current = call_id;
    tokenRef.current = call_token;

    const socket = new CallSocket(wsUrl(`/ws/caller/${encodeURIComponent(call_id)}?token=${encodeURIComponent(call_token)}`), {
      autoReconnect: true,
      onEvent: (msg: ServerEvent) => void handleEvent(msg),
      // undo the "Reconnecting…" label once the socket is back
      onOpen: () => {
        if (phaseRef.current === 'incall') setStatus('Connected');
        else if (phaseRef.current !== 'dialer') setStatus('Connecting…');
      },
      onClose: (byUser) => {
        if (!byUser && callIdRef.current) setStatus('Reconnecting…');
      },
    });
    socketRef.current = socket as CallSocket<unknown>;
    socket.connect();
    startTimer();
    setStatus('Connecting…');
    void runIvr();
  }

  // greeting ("Namaste…") -> beep -> record -> confirmation -> hold music
  async function runIvr() {
    if (ivrRunningRef.current) return;
    const gen = ++ivrGenRef.current;
    const alive = () => gen === ivrGenRef.current && ivrRunningRef.current;
    // Register each clip the moment it's created — not when the await
    // resolves — so a mid-clip hangup can pause it immediately.
    const onStart = (el: HTMLAudioElement) => {
      if (alive()) ivrElemsRef.current.add(el);
      else stopAudio(el);
    };
    ivrRunningRef.current = true;
    ivrDoneRef.current = false;
    setPhase('greeting');
    setStatus('Connecting…');
    const greeting = await playFirst(['/audio/greeting.mp3'], 25000, undefined, onStart);
    if (!alive()) return;
    stopAudio(greeting);
    if (greeting) ivrElemsRef.current.delete(greeting);

    await playOnce('/audio/beep.wav', 5000, undefined, onStart);
    if (!alive()) return;
    stopIvrAudio();
    beginRecording();
  }

  function beginRecording() {
    const stream = streamRef.current;
    const callId = callIdRef.current;
    const token = tokenRef.current;
    if (!stream || !callId || !token) return;
    setPhase('recording');
    setStatus('Speak your message now');
    setMsgLeft(MESSAGE_MAX_MS / 1000);
    pcmChunksRef.current = [];
    recorderRef.current = startCallRecorder(stream);
    let left = MESSAGE_MAX_MS / 1000;
    msgTimerRef.current = setInterval(() => {
      left -= 1;
      setMsgLeft(Math.max(0, left));
    }, 1000);
    watcherStopRef.current = startSilenceWatcher(stream, {
      silenceMs: SILENCE_MS,
      minMs: MIN_MSG_MS,
      maxMs: MESSAGE_MAX_MS,
      onLevel: setLevel,
      onDone: () => void finishMessage(),
    });
    // start streaming raw PCM for live emotion analysis right after the beep;
    // it keeps running through the hold phase until an officer picks up
    startPcm();
  }

  async function finishMessage() {
    if (ivrDoneRef.current) return; // silence watcher + max cap can both fire
    if (endingRef.current) return; // user already hung up — nothing to finish
    ivrDoneRef.current = true;
    stopWatcher();
    const callId = callIdRef.current;
    const token = tokenRef.current;
    const rec = recorderRef.current;
    recorderRef.current = null;
    if (callId && token && rec) {
      await uploadMessage(rec, callId, token);
    }
    if (!callIdRef.current) return; // call already ended
    // audible confirmation, then hold music while waiting for an officer
    setPhase('waiting');
    setStatus('Message recorded ✓');
    const conf = await playFirst(['/audio/confirmation.mp3'], 12000, undefined, (el) => {
      if (callIdRef.current) ivrElemsRef.current.add(el);
      else stopAudio(el);
    });
    if (!callIdRef.current) return;
    stopAudio(conf);
    if (conf) ivrElemsRef.current.delete(conf);
    ivrAudioRef.current = trackIvr(playLoopFirst(['/audio/hold_music.mp3']));
    socketRef.current?.send({ action: 'IVR_FINISHED', call_id: callId });
    setStatus('Waiting for an officer…');
  }

  /** Upload any in-flight message recording and tear down the peer, so a
   *  re-claimed call (agent released) can establish a fresh connection. */
  async function teardownPeer() {
    const rec = recorderRef.current;
    recorderRef.current = null;
    try { pcRef.current?.close(); } catch { /* ignore */ }
    pcRef.current = null;
    bufferedRef.current = [];
    const callId = callIdRef.current;
    const token = tokenRef.current;
    if (callId && token && rec) await stopAndUpload(rec, callId, token, 'caller_audio');
  }

  async function handleEvent(msg: ServerEvent) {
    switch (msg.event) {
      case 'AGENT_ASSIGNED': {
        // If an officer somehow claimed while the message window was still
        // open, finalize the message recording first so it isn't orphaned.
        if (phaseRef.current === 'recording') {
          stopWatcher();
          const rec = recorderRef.current;
          recorderRef.current = null;
          const cid = callIdRef.current;
          const tok = tokenRef.current;
          if (cid && tok && rec) await uploadMessage(rec, cid, tok);
        }
        stopIvrAudio();
        stopPcm(); // officer picked up — live emotion streaming ends here
        setStatus('Agent connecting…');
        await startPeer();
        break;
      }
      case 'WEBRTC_SIGNAL': {
        const pc = pcRef.current;
        if (!pc) break;
        try {
          const payload = msg.payload as { offer?: RTCSessionDescriptionInit; answer?: RTCSessionDescriptionInit; candidate?: RTCIceCandidateInit };
          await applyRemote(pc, payload, bufferedRef.current);
        } catch (e) {
          console.error('[caller] failed to apply remote signal', e);
        }
        break;
      }
      case 'CALL_RELEASED':
        // Agent stepped away: tear down the peer so a new claim can reconnect,
        // then go back to hold music.
        void (async () => {
          await teardownPeer();
          if (!callIdRef.current) return;
          stopIvrAudio();
          setPhase('waiting');
          setStatus('Agent stepped away — waiting for dispatch…');
          ivrAudioRef.current = trackIvr(playLoopFirst(['/audio/hold_music.mp3']));
          startPcm(); // back in queue — resume live emotion streaming
        })();
        break;
      case 'CALL_ENDED':
        await endCall(false);
        break;
      default:
        break;
    }
  }

  async function startPeer() {
    const stream = streamRef.current;
    const socket = socketRef.current;
    const callId = callIdRef.current;
    if (!stream || !socket || !callId || pcRef.current) return;
    const sendOffer = async (iceRestart = false) => {
      const pc = pcRef.current!;
      const offer = await pc.createOffer({ offerToReceiveAudio: true, iceRestart });
      await pc.setLocalDescription(offer);
      socket.send({ action: 'WEBRTC_SIGNAL', call_id: callId, payload: { offer: pc.localDescription!.toJSON() } });
    };
    const pc = await createPeer({
      localStream: stream,
      onRemoteStream: (remote) => {
        stopIvrAudio();
        const el = remoteAudioRef.current;
        if (el) {
          el.srcObject = remote;
          el.volume = 1;
          el.muted = false;
          const p = el.play();
          if (p) p.catch((e: DOMException) => {
            if (e?.name === 'NotAllowedError') {
              // ontrack fires long after the click — resume on next tap
              const retry = () => {
                window.removeEventListener('pointerdown', retry);
                el.play().catch(() => {});
              };
              window.addEventListener('pointerdown', retry);
              setStatus('Tap anywhere to enable audio');
            }
          });
        }
        setPhase('incall');
        setStatus('Connected');
        recorderRef.current = startCallRecorder(stream);
      },
      onSignal: (payload) => socket.send({ action: 'WEBRTC_SIGNAL', call_id: callId, payload }),
      onStateChange: (s) => {
        if (s === 'failed' || s === 'disconnected') setStatus('Connection lost…');
      },
      onIceFailed: () => {
        // caller is the offerer: re-offer with iceRestart to recover the media path
        sendOffer(true).catch((e) => console.error('[caller] ICE restart offer failed', e));
      },
    });
    pcRef.current = pc;
    // Caller is the OFFERER
    try {
      await sendOffer(false);
    } catch (e) {
      console.error('[caller] failed to create/send offer', e);
      setStatus('Could not start audio — try again');
    }
  }

  async function endCall(notify = true) {
    endingRef.current = true; // before stopWatcher, so finishMessage can't fire
    const callId = callIdRef.current;
    const token = tokenRef.current;
    const rec = recorderRef.current;
    recorderRef.current = null;
    stopWatcher();
    if (notify && callId) socketRef.current?.send({ action: 'END_CALL', call_id: callId });
    cleanup();
    callIdRef.current = null;
    tokenRef.current = null;
    if (callId && token && rec) {
      if (phaseRef.current === 'recording') await uploadMessage(rec, callId, token);
      else await stopAndUpload(rec, callId, token, 'caller_audio');
    }
    // back to the dialer, fresh state for the next call
    startingRef.current = false;
    setMuted(false);
    setLevel(0);
    setMsgLeft(MESSAGE_MAX_MS / 1000);
    setPhase('dialer');
    setStatus('Tap call to reach support');
  }

  function toggleMute() {
    const stream = streamRef.current;
    if (!stream) return;
    const next = !muted;
    stream.getAudioTracks().forEach((t) => { t.enabled = !next; });
    setMuted(next);
  }

  function pressKey(key: string) {
    // only the dialer accepts keys (in-call DTMF has no receiver — removed)
    if (phase === 'dialer') setDialed((v) => (v + key).slice(0, 16));
  }

  function backspace() {
    setDialed((v) => v.slice(0, -1));
  }

  return (
    <div className="call-screen">
      {phase === 'dialer' ? (
        <div className="dialer">
          <div className="dialer-number">{dialed || ' '}</div>
          <div className="dialer-add">＋ Add Number</div>
          <div className="dialer-grid">
            {KEYS.map((k) => (
              <button key={k} className="dial-key" onClick={() => pressKey(k)}>
                <span className="dial-digit">{k}</span>
                {KEY_LETTERS[k] && <span className="dial-letters">{KEY_LETTERS[k]}</span>}
              </button>
            ))}
          </div>
          <div className="dialer-row">
            <span />
            <button className="dial-call" onClick={() => void startCall()} aria-label="Call">
              <svg viewBox="0 0 24 24" width="30" height="30" fill="currentColor">
                <path d="M6.6 10.8c1.4 2.8 3.8 5.1 6.6 6.6l2.2-2.2c.3-.3.7-.4 1-.2 1.1.4 2.3.6 3.6.6.6 0 1 .4 1 1V20c0 .6-.4 1-1 1C10.6 21 3 13.4 3 4c0-.6.4-1 1-1h3.5c.6 0 1 .4 1 1 0 1.2.2 2.4.6 3.6.1.3 0 .7-.2 1l-2.3 2.2z" />
              </svg>
            </button>
            <button className="dial-key back" onClick={backspace} aria-label="Delete">
              <svg viewBox="0 0 24 24" width="26" height="26" fill="none" stroke="currentColor" strokeWidth="1.6">
                <path d="M21 6H8l-5 6 5 6h13a1 1 0 0 0 1-1V7a1 1 0 0 0-1-1z" />
                <line x1="18" y1="9" x2="12" y2="15" />
                <line x1="12" y1="9" x2="18" y2="15" />
              </svg>
            </button>
          </div>
        </div>
      ) : (
        <>
      <header className="call-header">
        <div className="call-name">{dialed || 'Customer Care'}</div>
        <div className="call-timer">{timer}</div>
        <div className="call-status">{status}</div>
      </header>

      <div className="caption">{PHASE_TEXT[phase]}</div>

      {phase === 'recording' && (
        <div className="msg-window">
          <div className="msg-meter"><div className="msg-meter-fill" style={{ width: `${level * 100}%` }} /></div>
          <div className="msg-left">min 3s · stops ~2s after you pause{msgLeft < MESSAGE_MAX_MS / 1000 ? ` · ${msgLeft}s cap left` : ''}</div>
        </div>
      )}

      <div className="orb-stage">
        <CrystalizedBall
          preset="abyss"
          particleCount={8000}
          size={0.62}
          speed={phase === 'incall' ? 1.4 : phase === 'recording' ? 1.1 : 0.8}
          interactive
        />
      </div>

      <audio ref={remoteAudioRef} autoPlay playsInline className="hidden-audio" />

      <div className="controls">
        {phase === 'incall' && (
          <button className={`ctrl${muted ? ' active' : ''}`} onClick={toggleMute} aria-label="Mute">
            {muted ? (              <svg viewBox="0 0 24 24" width="26" height="26" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                <line x1="2" y1="2" x2="22" y2="22" />
                <path d="M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V6a3 3 0 0 0-5.94-.6" />
                <path d="M17 11a3 3 0 0 1-3 3H9" />
                <line x1="12" y1="14" x2="12" y2="18" />
                <line x1="8" y1="18" x2="16" y2="18" />
              </svg>
            ) : (
              <svg viewBox="0 0 24 24" width="26" height="26" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                <path d="M12 2a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z" />
                <path d="M19 11a7 7 0 0 1-14 0" />
                <line x1="12" y1="18" x2="12" y2="22" />
              </svg>
            )}
          </button>
        )}

        {/* pointerdown stops IVR audio BEFORE the click — otherwise the
            tap-to-unlock retry armed for a blocked clip fires on this very
            tap and the clip blips at cut time */}
        <button className="ctrl end" onPointerDown={() => stopIvrAudio()} onClick={() => void endCall(true)} aria-label="End call">
          <svg viewBox="0 0 24 24" width="28" height="28" fill="currentColor">
            <path d="M12 9c-1.6 0-3.15.25-4.6.72v3.1c0 .39-.23.74-.56.9-.98.49-1.87 1.12-2.66 1.85-.18.18-.43.28-.7.28-.28 0-.53-.11-.71-.29L.29 13.08c-.18-.17-.29-.42-.29-.7 0-.28.11-.53.29-.71C3.34 8.78 7.46 7 12 7s8.66 1.78 11.71 4.67c.18.18.29.43.29.71 0 .28-.11.53-.29.7l-2.48 2.48c-.18.18-.43.29-.71.29-.27 0-.52-.1-.7-.28-.79-.73-1.68-1.36-2.66-1.85-.33-.16-.56-.51-.56-.9v-3.1C15.15 9.25 13.6 9 12 9z" />
          </svg>
        </button>
      </div>
        </>
      )}

      <Link to="/agent" target="_blank" rel="noreferrer" className="agent-link">Agent console →</Link>
    </div>
  );
}
